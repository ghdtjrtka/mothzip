@echo off
"%~dp0mothzip.exe" install
if errorlevel 1 (
  echo Installation failed. See the error above.
  pause
  exit /b 1
)
echo MothZip is installed. Open a new terminal to use the mothzip command.
echo To remove it, use Windows Settings - Apps - MothZip.
pause
