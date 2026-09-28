param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^COM[0-9]+$')]
    [string]$Port
)

$ErrorActionPreference = 'Stop'
for ($attempt = 1; $attempt -le 6; $attempt++) {
    $serial = [System.IO.Ports.SerialPort]::new($Port, 115200)
    $serial.DtrEnable = $false
    $serial.RtsEnable = $false
    $serial.WriteTimeout = 1000
    try {
        $serial.Open()
        $serial.DiscardInBuffer()
        # Ctrl+U clears any partial line left in the firmware console.
        for ($ping = 0; $ping -lt 2; $ping++) {
            $serial.Write(([char]21).ToString() + "PING`r")
            $response = ''
            for ($poll = 0; $poll -lt 10; $poll++) {
                Start-Sleep -Milliseconds 150
                $response += $serial.ReadExisting()
                if ($response.Contains('[rat] OK PONG protocol=1')) {
                    Write-Output "Firmware boot confirmed on ${Port}: PING answered."
                    exit 0
                }
            }
        }
    } catch {
        # USB Serial/JTAG may briefly re-enumerate after the watchdog reset.
    } finally {
        if ($serial.IsOpen) { $serial.Close() }
        $serial.Dispose()
    }
    Start-Sleep -Milliseconds 500
}
Write-Error "No firmware PING response on $Port after reset."
exit 1
