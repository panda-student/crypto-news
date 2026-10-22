!define CRYPTO_SHUTDOWN_SCRIPT "${__FILEDIR__}\shutdown.ps1"
!macro CryptoCloseInstalledApp executablePath
  InitPluginsDir
  File /oname=$PLUGINSDIR\crypto-shutdown.ps1 "${CRYPTO_SHUTDOWN_SCRIPT}"
  DetailPrint "正在关闭已安装的消息面分析…"
  nsExec::ExecToStack /TIMEOUT=20000 '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File "$PLUGINSDIR\crypto-shutdown.ps1" -ExecutablePath "${executablePath}"'
  Pop $0
  Pop $1
  ${If} $0 != 0
    MessageBox MB_ICONSTOP|MB_OK "无法关闭已安装的消息面分析，请通过托盘退出后重试。"
    Abort
  ${EndIf}
!macroend
