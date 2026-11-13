@echo off
chcp 65001 >nul
setlocal
title HGhub dev

cd /d "%~dp0"

set "APPLOG=%APPDATA%\com.hghub.huangguo\debug.log"
rem 局域网 IP 检测（192.168 优先；通配符必须带引号，否则 PowerShell 脚本块内解析报错）
powershell -NoProfile -Command "(Get-NetIPAddress -AddressFamily IPv4 | Where-Object IPAddress -NotLike '127.*' | Where-Object IPAddress -NotLike '169.254.*' | Sort-Object { if ($_.IPAddress -like '192.168.*') { 0 } elseif ($_.IPAddress -like '10.*' -or $_.IPAddress -like '172.*') { 1 } else { 2 } } | Select-Object -First 1).IPAddress | Out-File -Encoding ascii temp_lanip.txt"
set /p LANIP=<temp_lanip.txt
del temp_lanip.txt 2>nul
if not defined LANIP set "LANIP=未检测到"


echo ==================================================
echo  HGhub 开发模式启动 (Tauri 2 + Vite + Rust)
echo ==================================================
echo.
echo  [本机访问地址]
echo    前端 dev server     http://localhost:1420
echo    浏览器访问          http://localhost:1420  (与桌面窗口共用后端)
echo    局域网访问          http://%LANIP%:17650  (手机/其他电脑 均可)
echo    [提示] 局域网设备 首次访问时 Windows 会弹访问授权，选「允许」即可。
echo    本机桥 API/媒体      http://127.0.0.1:17650
echo    媒体代理-封面解密   http://hg.localhost/img?u=...
echo    媒体代理-播放列表   http://hg.localhost/hls?u=...
echo    媒体代理-分片透传   http://hg.localhost/seg?u=...
echo    应用日志            %APPLOG%
echo.
echo  [上游访问地址 - 实际线路由应用自动选线决定]
echo    默认 API 线路       https://huangguoai.com
echo    备用 API 线路       https://huangguoai.ai
echo    地址发布页          https://huangguoai.pages.dev/publish.js
echo    动态镜像示例        https://thu.adihsppe.cc  (子域随机)
echo    图片 CDN            https://pic.drleao.cn
echo    视频/密钥 CDN       https://tp*.drleao.cn  https://yd-lphu-*.hzjvlo.cn
echo    更新检查            https://api.github.com/repos/zxsos/HGhub/releases
echo.
echo  [说明] 播放与选线的实时 URL 由下方最小化的 HGhub-debug-log
echo         窗口输出（play / hls / seg / line 各行）；关闭主窗口后
echo         该日志窗口会自动关闭。
echo.

rem 局域网 IP 检测（192.168 优先；通配符必须带引号，否则 PowerShell 脚本块内解析报错）
powershell -NoProfile -Command "(Get-NetIPAddress -AddressFamily IPv4 | Where-Object IPAddress -NotLike '127.*' | Where-Object IPAddress -NotLike '169.254.*' | Sort-Object { if ($_.IPAddress -like '192.168.*') { 0 } elseif ($_.IPAddress -like '10.*' -or $_.IPAddress -like '172.*') { 1 } else { 2 } } | Select-Object -First 1).IPAddress | Out-File -Encoding ascii temp_lanip.txt"
set /p LANIP=<temp_lanip.txt
del temp_lanip.txt 2>nul
if not defined LANIP set "LANIP=未检测到"
netstat -ano | findstr /C:":1420 " | findstr "LISTENING" >nul 2>&1
if %errorlevel%==0 (
    echo [warn] 端口 1420 已被占用，可能已有 HGhub dev 实例在运行。
    echo        请先关闭现有实例或其 vite 进程，再运行本脚本。
    echo.
    pause
    exit /b 1
)

if not exist node_modules (
    echo [setup] node_modules 不存在，正在安装依赖（首次较慢）...
    call pnpm install
    if errorlevel 1 (
        echo [error] 依赖安装失败，请检查网络或 Node/pnpm 环境。
        pause
        exit /b 1
    )
)

rem 编码说明: 本文件保存为 UTF-8（无 BOM），第 2 行 chcp 65001 切换控制台代码页；勿删 chcp 行，否则中文乱码
rem 独立窗口实时跟踪应用日志：输出所有 play / hls / seg / line 访问 URL
rem 先清扫历史遗留的日志跟踪窗口（排除自身进程，避免误杀）
powershell -NoProfile -Command "Get-CimInstance Win32_Process -Filter 'Name=''powershell.exe''' | Where-Object { $_.ProcessId -ne $PID -and $_.CommandLine -like '*Get-Content*com.hghub.huangguo*debug.log*Wait*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }"
rem 启动新日志跟踪窗口并记录其 PID，供应用退出后自动关闭
set "TAILPIDFILE=%TEMP%\hghub_tail_pid.txt"
del "%TAILPIDFILE%" 2>nul
start "HGhub-debug-log" /min powershell -NoProfile -Command "$PID | Out-File '%TAILPIDFILE%' -Encoding ascii; while(-not (Test-Path '%APPLOG%')){ Start-Sleep -Milliseconds 500 }; Get-Content '%APPLOG%' -Wait -Tail 50"
set /a TAILWAIT=0
:wait_tailpid
if exist "%TAILPIDFILE%" goto tailpid_ok
set /a TAILWAIT+=1
if %TAILWAIT% lss 20 (
    ping -n 1 127.0.0.1 >nul
    goto wait_tailpid
)
:tailpid_ok
if exist "%TAILPIDFILE%" set /p TAILPID=<"%TAILPIDFILE%"

if not exist dist (
    echo [setup] 未找到 dist 产物，正在为页面入口构建……
    call pnpm build
    if errorlevel 1 (
        echo [error] 构建失败，请检查环境
        pause
        exit /b 1
    )
)
rem --prebuilt 复用刚验证过的本地 debug 程序，仅启动 Vite，不触发重复 Rust 编译。
rem 本机健康探针显式绕过系统代理，避免 localhost 被代理造成假超时。
rem 修改 Rust 后须先 cargo build；默认启动方式保持不变。
if /I "%~1"=="--prebuilt" (
    if not exist src-tauri\target\debug\huangguo-client.exe (
        echo [error] 未找到本地 debug 程序，请先运行 cargo build --manifest-path src-tauri/Cargo.toml -j 1
        exit /b 1
    )
    echo [run] 已构建程序 + Vite（关闭桌面窗口时自动停止本次 Vite）
    powershell -NoProfile -Command "$ErrorActionPreference = 'Stop'; $result = 1; $vite = Start-Process -FilePath 'node' -ArgumentList 'node_modules/vite/bin/vite.js' -WorkingDirectory '%~dp0' -PassThru; try { $deadline = (Get-Date).AddSeconds(15); while ($true) { if ($vite.HasExited) { throw 'Vite 启动失败' }; try { $probe = [Net.WebRequest]::Create('http://localhost:1420/'); $probe.Proxy = $null; $probe.Timeout = 1000; $response = $probe.GetResponse(); $ready = [int]$response.StatusCode -eq 200; $response.Close(); if ($ready) { break } } catch {}; if ((Get-Date) -gt $deadline) { throw 'Vite 启动超时' }; Start-Sleep -Milliseconds 150 }; Push-Location '%~dp0src-tauri'; .\target\debug\huangguo-client.exe; $result = $LASTEXITCODE; Pop-Location } finally { if (-not $vite.HasExited) { Stop-Process -Id $vite.Id -Force -ErrorAction SilentlyContinue } }; exit $result"
) else (
    echo [run] pnpm app:dev  （关闭应用窗口即退出；Ctrl+C 可中断）
    call pnpm app:dev
)
set "RUNEXIT=%errorlevel%"

rem 应用退出后自动关闭日志跟踪窗口（Ctrl+C 强退时来不及执行，下次启动的清扫会兜底）
if defined TAILPID taskkill /PID %TAILPID% /F >nul 2>&1
del "%TAILPIDFILE%" 2>nul

echo.
echo [exit] HGhub dev 已退出，日志跟踪窗口已自动关闭。
pause
endlocal & exit /b %RUNEXIT%
