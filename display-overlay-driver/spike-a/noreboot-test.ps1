# No-reboot hypothesis test: can a driver signed by a STANDARD (non-test) self-signed
# root load in the currently-running kernel (booted with test-signing OFF)?
# If yes -> production-signing with a self-signed root avoids the reboot.
# If 577 again -> the kernel caches its trust list at boot; reboot is genuinely required.
$here = "C:\Users\punk\Documents\mine\natively-cluely-ai-assistant\display-overlay-driver\spike-a"
$sys  = "$here\bin\x64\Release\NativelyOverlay.sys"
$copy = "$here\bin\x64\Release\NativelyOverlayNR2.sys"
Copy-Item $sys $copy -Force

# 1. CAPI key with AT_SIGNATURE key spec (what signtool code-signing accepts),
#    self-signed code-signing root (NOT added to the Windows Test Root group).
$csp = New-Object System.Security.Cryptography.CspParameters
$csp.KeyContainerName = "NoRebootStdKey"
$csp.KeyNumber = [System.Security.Cryptography.KeyNumber]::Signature
$rsa = New-Object System.Security.Cryptography.RSACryptoServiceProvider(2048, $csp)
$eku = New-Object System.Security.Cryptography.OidCollection
$eku.Add("1.3.6.1.4.1.311.10.3.15")
$hash = [System.Security.Cryptography.HashAlgorithmName]::SHA256
$pad  = [System.Security.Cryptography.RSASignaturePadding]::Pkcs1
$req = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new("CN=NoRebootStdRoot", $rsa, $hash, $pad)
$req.CertificateExtensions.Add([System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new($eku, $false))
$req.CertificateExtensions.Add([System.Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new([System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature, $true))
$cert = $req.CreateSelfSigned((Get-Date).AddDays(-1), (Get-Date).AddYears(5))

# 2. Key into CurrentUser\My (where signtool looks), trust in LocalMachine\Root.
$my = New-Object System.Security.Cryptography.X509Certificates.X509Store("My","CurrentUser")
$my.Open("ReadWrite"); $my.Add($cert); $my.Close()
$root = New-Object System.Security.Cryptography.X509Certificates.X509Store("Root","LocalMachine")
$root.Open("ReadWrite"); $root.Add($cert); $root.Close()
Write-Host "std root: $($cert.Thumbprint)  HasKey=$($cert.HasPrivateKey)"

# 3. Sign the copy (no timestamp).
$signtool = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter signtool.exe | Where-Object { $_.FullName -match '\\x64\\' } | Select-Object -First 1 -ExpandProperty FullName
& $signtool sign /sha1 $cert.Thumbprint /fd SHA256 $copy 2>&1 | Out-Host
Write-Host "sign exit: $LASTEXITCODE"

# 4. Load test: running kernel is in PRODUCTION mode (test-signing off at boot).
$svc = "NativelyOverlayNR2"
sc.exe stop $svc 2>&1 | Out-Host
sc.exe delete $svc 2>&1 | Out-Host
sc.exe create $svc type= kernel binPath= "$copy" start= demand 2>&1 | Out-Host
& sc.exe start $svc 2>&1 | Out-Host
Write-Host "start exit: $LASTEXITCODE"
sc.exe query $svc 2>&1 | Out-Host

# 5. CI verdict for this attempt.
Start-Sleep -Milliseconds 800
$e = Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-CodeIntegrity/Operational'; Id=3089; StartTime=(Get-Date).AddMinutes(-2)} -ErrorAction Stop | Select-Object -First 1
if ($e) {
  $x = [xml]$e.ToXml()
  foreach ($n in $x.SelectNodes("//*[local-name()='Data']")) {
    $nm = $n.GetAttribute('Name'); if ($nm -and $n.InnerText) { Write-Host ("  {0,-22} {1}" -f $nm, $n.InnerText) }
  }
}

# 6. Cleanup.
sc.exe stop $svc 2>&1 | Out-Host
sc.exe delete $svc 2>&1 | Out-Host
Remove-Item $copy -Force
Write-Host "done"