import { execFile } from 'node:child_process';
import { chmod, mkdir, mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { promisify } from 'node:util';

export async function privateDirectory() {
  const dir = await mkdtemp(join(tmpdir(), 'devlink-test-'));
  if (process.platform === 'win32') {
    await promisify(execFile)('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', `
$ErrorActionPreference = 'Stop'
$sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$acl = New-Object System.Security.AccessControl.DirectorySecurity
$acl.SetOwner($sid)
$acl.SetAccessRuleProtection($true, $false)
$rule = New-Object System.Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
$acl.AddAccessRule($rule)
[System.IO.Directory]::SetAccessControl($env:DEVLINK_TEST_DIR, $acl)
`], { env: { ...process.env, DEVLINK_TEST_DIR: dir }, windowsHide: true });
  } else await chmod(dir, 0o700);
  return { dir, cleanup: () => rm(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }) };
}

export async function privateSettings(settings, profile) {
  const { dir, cleanup } = await privateDirectory();
  const path = profile ? join(dir, '.devlink-mcp', profile, 'broker.json') : join(dir, 'broker.json');
  if (profile) await mkdir(join(dir, '.devlink-mcp', profile), { recursive: true, mode: 0o700 });
  await writeFile(path, JSON.stringify(settings), { mode: 0o600 });
  return { path, cleanup };
}

export async function allowOtherUserRead(path) {
  if (process.platform !== 'win32') return chmod(path, 0o644);
  await promisify(execFile)('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', `
$ErrorActionPreference = 'Stop'
$p = $env:DEVLINK_TEST_FILE
$acl = [System.IO.File]::GetAccessControl($p)
$world = New-Object System.Security.Principal.SecurityIdentifier('S-1-1-0')
$acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($world, 'Read', 'Allow')))
[System.IO.File]::SetAccessControl($p, $acl)
`], { env: { ...process.env, DEVLINK_TEST_FILE: path }, windowsHide: true });
}
