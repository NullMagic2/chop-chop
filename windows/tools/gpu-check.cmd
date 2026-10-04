@echo off
rem Chop Chop Splitter - GPU encoder check.
rem Runs the same short test encodes the app uses to pick an encoder, with the
rem FFmpeg that the installer put next to chop-chop.exe, then times each encoder.
setlocal
rem The 64-bit Program Files folder, even from a 32-bit prompt; else next to this script.
set "FFMPEG=%ProgramW6432%\Chop Chop Splitter\ffmpeg.exe"
if not exist "%FFMPEG%" set "FFMPEG=%ProgramFiles%\Chop Chop Splitter\ffmpeg.exe"
if not exist "%FFMPEG%" set "FFMPEG=%~dp0ffmpeg.exe"
if not exist "%FFMPEG%" (
  echo ffmpeg.exe not found. Install Chop Chop Splitter first,
  echo or put this script next to ffmpeg.exe.
  pause
  exit /b 1
)

set "CPU=-c:v libx264 -preset fast -crf 20 -pix_fmt yuv420p"
set "NVENC=-c:v h264_nvenc -preset p5 -tune hq -rc vbr -cq 21 -b:v 0 -profile:v high -pix_fmt yuv420p"
set "QSV=-c:v h264_qsv -preset medium -global_quality 21 -pix_fmt nv12"
set "AMF=-c:v h264_amf -quality balanced -rc cqp -qp_i 20 -qp_p 22 -qp_b 24 -pix_fmt nv12"

echo === Graphics adapters ===
powershell -NoProfile -Command "Get-CimInstance Win32_VideoController | ForEach-Object { '{0}   driver {1}' -f $_.Name, $_.DriverVersion }"
echo.
echo === Hardware encoder test (what the app runs at startup) ===
call :test "NVIDIA NVENC    " "%NVENC%"
call :test "Intel Quick Sync" "%QSV%"
call :test "AMD AMF         " "%AMF%"
echo.
echo === Speed: 20 s of 1080p video (lower is faster) ===
call :speed "CPU, libx264    " "%CPU%"
call :speed "NVIDIA NVENC    " "%NVENC%"
call :speed "Intel Quick Sync" "%QSV%"
call :speed "AMD AMF         " "%AMF%"
echo.
echo Copy everything above and send it back.
pause
exit /b 0

:test
"%FFMPEG%" -hide_banner -nostdin -loglevel error -f lavfi -i testsrc2=size=640x360:rate=30 -frames:v 10 %~2 -f null - 2>"%TEMP%\ccs-gpu.txt"
rem FFmpeg exits with -1 on failure, which "if errorlevel 1" misses (it means >= 1).
set "RC=%errorlevel%"
if "%RC%"=="0" goto test_ok
echo %~1  NOT AVAILABLE
for /f "usebackq delims=" %%L in ("%TEMP%\ccs-gpu.txt") do echo         %%L
exit /b 0
:test_ok
echo %~1  OK
exit /b 0

:speed
"%FFMPEG%" -hide_banner -nostdin -loglevel error -f lavfi -i testsrc2=size=640x360:rate=30 -frames:v 10 %~2 -f null - >nul 2>&1
rem FFmpeg exits with -1 on failure, which "if errorlevel 1" misses (it means >= 1).
set "RC=%errorlevel%"
if "%RC%"=="0" goto speed_run
echo %~1  skipped, not available
exit /b 0
:speed_run
for /f "usebackq delims=" %%S in (`powershell -NoProfile -Command "$a = '-hide_banner -nostdin -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 20 %~2 -f null -'; $t = Measure-Command { Start-Process -FilePath $env:FFMPEG -ArgumentList $a -NoNewWindow -Wait }; '{0:N1} s' -f $t.TotalSeconds"`) do echo %~1  %%S
exit /b 0
