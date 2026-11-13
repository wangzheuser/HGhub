param([int]$AppProcessId = 0, [string]$OutputDirectory = '')
$ErrorActionPreference = 'Stop'
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $PSScriptRoot ("../test-results/normal-desktop/" + (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH-mm-ss-fffZ')) }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
if (Test-Path (Join-Path $OutputDirectory 'report.json')) { throw 'Report already exists' }
$report = [ordered]@{ started=(Get-Date -Format o); observation='Owned Tauri window, Windows UI Automation; no CDP, media or IPC mocking'; functional_pass=$false }
try {
    $expected = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../src-tauri/target/debug/huangguo-client.exe'))
    $apps = @(Get-CimInstance Win32_Process -Filter "Name='huangguo-client.exe'" | Where-Object { $_.ExecutablePath -eq $expected -and (-not $AppProcessId -or $_.ProcessId -eq $AppProcessId) })
    if ($apps.Count -ne 1) { throw 'Expected exactly one owned HGhub process' }
    $appId = [int]$apps[0].ProcessId
    $report.process_id = $appId
    $report.process_path_verified = $true
    $ports = @(Get-NetTCPConnection -State Listen | Where-Object LocalPort -in @(1420,17650,9227) | Select-Object LocalAddress,LocalPort,OwningProcess)
    $report.ports = $ports
    $report.temporary_debug_port_absent = @($ports | Where-Object LocalPort -eq 9227).Count -eq 0
    if (-not $report.temporary_debug_port_absent) { throw 'Temporary debug port remains open' }
    if (-not @($ports | Where-Object { $_.LocalPort -eq 17650 -and $_.OwningProcess -eq $appId }).Count) { throw 'Bridge not owned by expected app' }
    $report.frontend_status = [int](Invoke-WebRequest 'http://localhost:1420/' -NoProxy -TimeoutSec 5).StatusCode
    $stats = Invoke-RestMethod 'http://127.0.0.1:17650/api/cache_stats' -Method Post -ContentType 'application/json' -Body '{}' -NoProxy -TimeoutSec 10
    if (-not $stats.ok) { throw 'Cache status command failed' }
    $report.cache = $stats.data
    $logging = Invoke-RestMethod 'http://127.0.0.1:17650/api/playback_log_status' -Method Post -ContentType 'application/json' -Body '{}' -NoProxy -TimeoutSec 5
    if (-not $logging.ok) { throw 'Diagnostics status command failed' }
    $report.logging = $logging.data | Select-Object enabled,error
    if ($report.logging.enabled) { throw 'Diagnostics preference not restored' }
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes
    $process = Get-Process -Id $appId
    $deadline = (Get-Date).AddSeconds(30)
    while (-not $process.MainWindowHandle -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250; $process.Refresh() }
    if (-not $process.MainWindowHandle) { throw 'Owned native window missing' }
    $shell = New-Object -ComObject WScript.Shell
    $report.window_activated = $shell.AppActivate($appId)
    $window = [System.Windows.Automation.AutomationElement]::FromHandle($process.MainWindowHandle)
    function Get-PlaybackSnapshot {
        $sliderCondition = [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ControlTypeProperty,[System.Windows.Automation.ControlType]::Slider)
        $sliders = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants,$sliderCondition)
        $value = $null; $maximum = $null
        foreach ($slider in $sliders) {
            if ($slider.Current.Name -ne '播放进度' -or $slider.Current.IsOffscreen) { continue }
            $pattern = $null
            if ($slider.TryGetCurrentPattern([System.Windows.Automation.RangeValuePattern]::Pattern,[ref]$pattern)) { $value=$pattern.Current.Value; $maximum=$pattern.Current.Maximum; break }
        }
        # 桌面质量按钮显式 aria-label 覆盖可见文字；真实像素标签由 title 映射至 HelpText。
        $qualityCondition = [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty,'画质切换')
        $qualityControls = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants,$qualityCondition)
        $quality = @($qualityControls | ForEach-Object { $_.Current.HelpText } | Where-Object { $_ -match '^\d{3,4}p(?:\b|\s|·)' })
        [pscustomobject]@{ at=(Get-Date -Format o); progress=$value; maximum=$maximum; resolution_labels=$quality }
    }
    $deadline=(Get-Date).AddSeconds(50)
    do { $before=Get-PlaybackSnapshot; if ($null -ne $before.progress -and $before.maximum -gt 0 -and $before.resolution_labels.Count) { break }; Start-Sleep -Milliseconds 500 } while ((Get-Date) -lt $deadline)
    $report.before=$before
    Start-Sleep -Milliseconds 2200
    $after=Get-PlaybackSnapshot
    $report.after=$after
    $report.progress_advanced=($null -ne $before.progress -and $null -ne $after.progress -and $after.progress -gt $before.progress)
    $report.functional_pass=($report.frontend_status -eq 200 -and $report.progress_advanced -and $after.resolution_labels.Count -gt 0)
    $report.limitations=@('Progress and truthful pixel-label observation is not a video-frame counter; the separate live-steady report measures real rendered frames.','No claim of globally cold startup or all-source subsecond loading.')
} catch { $report.error=$_.Exception.Message -replace 'https?://[^\s"<>]+','[resource-url]' }
$report.finished=Get-Date -Format o
$report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'report.json') -Encoding utf8
$report | ConvertTo-Json -Depth 8
if ($report.functional_pass) { exit 0 } else { exit 1 }
