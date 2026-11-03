# GitHub Windows runner는 관리자 권한(elevated)으로 실행되어 새 파일의 기본 소유자가
# BUILTIN\Administrators가 된다. DevLink 비공개 파일 검증은 소유자가 현재 사용자 SID여야
# 하므로, 이 프로세스 토큰의 기본 소유자를 현재 사용자로 바꾼 뒤 명령을 자식 프로세스로
# 실행한다. 자식 프로세스는 바뀐 토큰을 복제해 받으므로 일반 사용자 환경과 같아진다.
#
# 사용: pwsh .github/scripts/invoke-as-user-owner.ps1 cargo test --workspace
$ErrorActionPreference = 'Stop'
if ($args.Count -eq 0) { throw 'Usage: invoke-as-user-owner.ps1 <command> [args...]' }

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;

public static class DevLinkTokenOwner {
  const uint TOKEN_QUERY = 0x0008;
  const uint TOKEN_ADJUST_DEFAULT = 0x0080;
  const int TokenOwner = 4;

  [StructLayout(LayoutKind.Sequential)]
  struct TOKEN_OWNER { public IntPtr Owner; }

  [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  [DllImport("advapi32.dll", SetLastError = true)]
  static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
  [DllImport("advapi32.dll", SetLastError = true)]
  static extern bool SetTokenInformation(IntPtr token, int infoClass, ref TOKEN_OWNER info, int length);

  public static void UseCurrentUser() {
    IntPtr token;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_ADJUST_DEFAULT, out token))
      throw new Win32Exception(Marshal.GetLastWin32Error());
    SecurityIdentifier sid = WindowsIdentity.GetCurrent().User;
    byte[] bytes = new byte[sid.BinaryLength];
    sid.GetBinaryForm(bytes, 0);
    IntPtr buffer = Marshal.AllocHGlobal(bytes.Length);
    try {
      Marshal.Copy(bytes, 0, buffer, bytes.Length);
      TOKEN_OWNER owner = new TOKEN_OWNER { Owner = buffer };
      if (!SetTokenInformation(token, TokenOwner, ref owner, Marshal.SizeOf(owner)))
        throw new Win32Exception(Marshal.GetLastWin32Error());
    } finally {
      Marshal.FreeHGlobal(buffer);
      CloseHandle(token);
    }
  }
}
'@

[DevLinkTokenOwner]::UseCurrentUser()

# 보정이 자식 프로세스까지 이어지는지, 자식 Windows PowerShell이 만든 새 파일의 소유자로 확인한다.
$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$probe = @'
$ProgressPreference = 'SilentlyContinue'
$p = Join-Path ([IO.Path]::GetTempPath()) ('devlink-owner-' + [guid]::NewGuid() + '.tmp')
[IO.File]::WriteAllText($p, '')
try { [Console]::Write([IO.File]::GetAccessControl($p).GetOwner([Security.Principal.SecurityIdentifier]).Value) }
finally { Remove-Item -LiteralPath $p -Force }
'@
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($probe))
$owner = (& powershell.exe -NoProfile -NonInteractive -EncodedCommand $encoded | Out-String).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Owner probe failed' }
if ($owner -ne $user) { throw "New files are owned by '$owner' instead of $user" }
Write-Host "New files are owned by the current user ($user)."

# npm.ps1 같은 PowerShell 래퍼는 '-'로 시작하는 인자를 매개변수로 해석하므로 실행 파일로 호출한다.
$command = (Get-Command $args[0] -CommandType Application | Select-Object -First 1).Source
[string[]]$rest = @($args | Select-Object -Skip 1)
& $command @rest
exit $LASTEXITCODE
