#!/usr/bin/env node
// devlink 실행 파일과 MCP 서버 런타임을 한 폴더로 묶어 릴리스 압축 파일을 만든다.
//
//   devlink-<version>-<target>/
//     devlink(.exe)
//     mcp-server/{package.json, dist/, node_modules/}
//
// setup은 실행 파일 옆의 mcp-server/dist/index.js를 MCP entry로 찾는다
// (rust/crates/devlink-broker/src/setup_mcp.rs의 runtime_entry). Node는 번들에 넣지 않고
// 사용자 PATH의 Node 24 이상을 쓴다.
//
// 사용:
//   node .github/scripts/package-release.mjs [--out-dir <DIR>] [--expect-version <vX.Y.Z>] [--no-smoke]
//   node .github/scripts/package-release.mjs --checksums <DIR>   # DIR의 압축 파일로 SHA256SUMS만 다시 만든다
//
// 원래 작업 트리의 packages/mcp-server(node_modules, dist)는 건드리지 않는다. MCP 서버는
// <out-dir>/.work 아래 복사본에서 설치·빌드한다. Rust는 평소처럼 rust/target에 release 빌드한다.
// Windows에서는 cargo +stable-x86_64-pc-windows-msvc를 쓴다. DEVLINK_CARGO_TOOLCHAIN으로
// 바꿀 수 있고, 빈 값이면 rust-toolchain.toml 기본 도구 체인을 쓴다.

import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const isWindows = process.platform === 'win32';
const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const rustDir = join(repoRoot, 'rust');
const mcpDir = join(repoRoot, 'packages', 'mcp-server');
const ARCHIVE_PATTERN = /^devlink-.+\.(zip|tar\.gz)$/;

function fail(message) {
  console.error(`package-release: ${message}`);
  process.exit(1);
}

function parseArgs(argv) {
  const options = { outDir: join(repoRoot, 'dist', 'release'), expectVersion: '', smoke: true, checksums: null };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) fail(`${arg} 값이 없습니다.`);
      i += 1;
      return argv[i];
    };
    if (arg === '--out-dir') options.outDir = resolve(value());
    else if (arg === '--expect-version') options.expectVersion = value().replace(/^v/, '');
    else if (arg === '--no-smoke') options.smoke = false;
    else if (arg === '--checksums') options.checksums = resolve(value());
    else fail(`알 수 없는 인자: ${arg}`);
  }
  return options;
}

function run(command, args, { cwd, capture = false, verbatim = false } = {}) {
  console.log(`> ${command} ${args.join(' ')}${cwd ? `  (${cwd})` : ''}`);
  const result = spawnSync(command, args, {
    cwd,
    windowsVerbatimArguments: verbatim,
    stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit',
    encoding: 'utf8',
  });
  if (result.error) fail(`${command} 실행 실패: ${result.error.message}`);
  if (result.status !== 0) fail(`${command} ${args.join(' ')} 실패 (종료 코드 ${result.status})`);
  return capture ? result.stdout : '';
}

// Windows의 npm은 npm.cmd라 cmd.exe를 거친다. 인자는 공백·특수 문자 없는 고정 플래그만 쓴다.
function npm(args, cwd) {
  if (isWindows) {
    run(process.env.ComSpec ?? 'cmd.exe', ['/d', '/s', '/c', `"npm ${args.join(' ')}"`], { cwd, verbatim: true });
  } else {
    run('npm', args, { cwd });
  }
}

function cargoToolchainArgs() {
  const configured = process.env.DEVLINK_CARGO_TOOLCHAIN;
  if (configured !== undefined) return configured ? [`+${configured.replace(/^\+/, '')}`] : [];
  return isWindows ? ['+stable-x86_64-pc-windows-msvc'] : [];
}

function workspaceVersion() {
  const manifest = readFileSync(join(rustDir, 'Cargo.toml'), 'utf8');
  const section = manifest.match(/^\[workspace\.package\][^[]*/m)?.[0] ?? '';
  const version = section.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  if (!version) fail('rust/Cargo.toml의 [workspace.package] version을 찾지 못했습니다.');
  return version;
}

function sha256(file) {
  return createHash('sha256').update(readFileSync(file)).digest('hex');
}

// names를 주면 그 파일만, 아니면 dir의 devlink 압축 파일 전체를 해시한다(릴리스 job의 병합용).
function writeChecksums(dir, names) {
  const archives = (names ?? readdirSync(dir).filter((name) => ARCHIVE_PATTERN.test(name))).sort();
  if (archives.length === 0) fail(`${dir}에 devlink 압축 파일이 없습니다.`);
  const lines = archives.map((name) => `${sha256(join(dir, name))}  ${name}\n`);
  writeFileSync(join(dir, 'SHA256SUMS'), lines.join(''));
  console.log(`SHA256SUMS (${archives.length}개)\n${lines.join('')}`);
}

function nodeMajor() {
  const version = run(process.execPath, ['--version'], { capture: true }).trim();
  return Number(version.replace(/^v/, '').split('.')[0]);
}

// 번들의 MCP 서버가 자격 파일 없이 initialize에 응답하는지 확인한다(setup의 실행 검사와 같은 조건).
function smokeMcp(entry, version) {
  return new Promise((resolvePromise, reject) => {
    const env = { ...process.env };
    delete env.DEVLINK_BROKER_FILE;
    delete env.NODE_OPTIONS;
    const child = spawn(process.execPath, [entry, '--transport', 'stdio'], { env, stdio: ['pipe', 'pipe', 'inherit'] });
    let buffer = '';
    let settled = false;
    const finish = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      child.stdin.end();
      child.kill();
      if (error) reject(error);
      else resolvePromise();
    };
    const timer = setTimeout(() => finish(new Error('MCP initialize 응답이 15초 안에 오지 않았습니다.')), 15_000);
    child.on('error', finish);
    child.stdin.on('error', () => {}); // 서버가 먼저 끝나면 EPIPE가 난다. 실패는 exit에서 보고한다.
    child.on('exit', (code) => finish(new Error(`MCP 서버가 응답 전에 종료됐습니다 (종료 코드 ${code}).`)));
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      buffer += chunk;
      let newline;
      while ((newline = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);
        if (!line) continue;
        let message;
        try {
          message = JSON.parse(line);
        } catch {
          finish(new Error(`MCP 서버가 JSON이 아닌 출력을 냈습니다: ${line.slice(0, 200)}`));
          return;
        }
        if (message.id !== 1) continue;
        const info = message.result?.serverInfo;
        if (info?.name !== 'devlink-mcp') {
          finish(new Error(`예상하지 못한 initialize 응답: ${line.slice(0, 200)}`));
        } else {
          console.log(`MCP initialize 확인: ${info.name} ${info.version}`);
          if (info.version !== version) console.warn(`경고: MCP serverInfo.version(${info.version})이 릴리스 버전(${version})과 다릅니다.`);
          finish();
        }
        return;
      }
    });
    child.stdin.write(`${JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: {
        protocolVersion: '2025-11-25',
        capabilities: {},
        clientInfo: { name: 'devlink-release-smoke', version },
      },
    })}\n`);
  });
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.checksums) {
    writeChecksums(options.checksums);
    return;
  }

  const version = workspaceVersion();
  const mcpPackage = JSON.parse(readFileSync(join(mcpDir, 'package.json'), 'utf8'));
  if (mcpPackage.version !== version) {
    fail(`버전 불일치: rust/Cargo.toml ${version}, packages/mcp-server/package.json ${mcpPackage.version}`);
  }
  if (options.expectVersion && options.expectVersion !== version) {
    fail(`태그 버전(${options.expectVersion})이 소스 버전(${version})과 다릅니다.`);
  }

  const toolchain = cargoToolchainArgs();
  const rustcInfo = run('rustc', [...toolchain, '-vV'], { cwd: rustDir, capture: true });
  const target = rustcInfo.match(/^host:\s*(\S+)/m)?.[1];
  if (!target) fail('rustc -vV에서 host target을 읽지 못했습니다.');

  const name = `devlink-${version}-${target}`;
  const workDir = join(options.outDir, '.work');
  const buildDir = join(workDir, 'mcp-build');
  const stageDir = join(workDir, name);
  const stageMcp = join(stageDir, 'mcp-server');
  const exeName = isWindows ? 'devlink.exe' : 'devlink';
  const archive = join(options.outDir, `${name}${isWindows ? '.zip' : '.tar.gz'}`);

  rmSync(workDir, { recursive: true, force: true });
  rmSync(archive, { force: true });
  mkdirSync(buildDir, { recursive: true });
  mkdirSync(stageMcp, { recursive: true });

  // 1. devlink release 빌드
  run('cargo', [...toolchain, 'build', '--release', '--locked', '-p', 'devlink-cli'], { cwd: rustDir });
  const targetDir = process.env.CARGO_TARGET_DIR ? resolve(rustDir, process.env.CARGO_TARGET_DIR) : join(rustDir, 'target');
  const binary = join(targetDir, 'release', exeName);
  if (!existsSync(binary)) fail(`빌드 산출물이 없습니다: ${binary}`);

  // 2. MCP 서버: 복사본에서 전체 의존성으로 빌드한다.
  for (const entry of ['package.json', 'package-lock.json', 'tsconfig.json', 'src']) {
    cpSync(join(mcpDir, entry), join(buildDir, entry), { recursive: true });
  }
  npm(['ci', '--no-audit', '--no-fund'], buildDir);
  npm(['run', 'build'], buildDir);

  // 3. 스테이징: 빌드 결과와 런타임 의존성만 둔다. 잠금 파일 그대로 설치한 뒤 package.json을 줄인다.
  cpSync(join(buildDir, 'dist'), join(stageMcp, 'dist'), { recursive: true });
  cpSync(join(mcpDir, 'package.json'), join(stageMcp, 'package.json'));
  cpSync(join(mcpDir, 'package-lock.json'), join(stageMcp, 'package-lock.json'));
  npm(['ci', '--omit=dev', '--ignore-scripts', '--no-audit', '--no-fund'], stageMcp);
  const runtimePackage = {
    name: mcpPackage.name,
    version: mcpPackage.version,
    private: true,
    type: mcpPackage.type,
    description: mcpPackage.description,
    engines: mcpPackage.engines,
    main: 'dist/index.js',
    dependencies: mcpPackage.dependencies,
  };
  writeFileSync(join(stageMcp, 'package.json'), `${JSON.stringify(runtimePackage, null, 2)}\n`);
  rmSync(join(stageMcp, 'package-lock.json'), { force: true });
  rmSync(join(stageMcp, 'node_modules', '.bin'), { recursive: true, force: true });

  cpSync(binary, join(stageDir, exeName));
  if (!isWindows) chmodSync(join(stageDir, exeName), 0o755);

  // 4. 구조 확인: setup이 찾는 entry와 런타임 의존성, 개발 의존성 제외
  const entry = join(stageMcp, 'dist', 'index.js');
  for (const required of [entry, join(stageMcp, 'node_modules', '@modelcontextprotocol', 'server'), join(stageMcp, 'node_modules', 'zod')]) {
    if (!existsSync(required)) fail(`번들에 필요한 파일이 없습니다: ${required}`);
  }
  for (const devOnly of ['typescript', '@types', join('@modelcontextprotocol', 'client')]) {
    if (existsSync(join(stageMcp, 'node_modules', devOnly))) fail(`번들에 개발 의존성이 남았습니다: ${devOnly}`);
  }

  // 5. 실행 확인
  if (options.smoke) {
    const reported = run(join(stageDir, exeName), ['--version'], { capture: true }).trim();
    if (reported !== `devlink ${version}`) fail(`devlink --version 출력이 예상과 다릅니다: ${reported}`);
    console.log(`실행 파일 확인: ${reported}`);
    const major = nodeMajor();
    if (major < 24) fail(`MCP 확인에는 Node 24 이상이 필요합니다 (현재 ${process.version}).`);
    await smokeMcp(entry, version);
  }

  // 6. 압축. Windows는 OS 내장 bsdtar로 zip을 만든다(PATH의 GNU tar는 zip을 못 만든다).
  if (isWindows) {
    const tar = join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'tar.exe');
    run(tar, ['-a', '-c', '-f', archive, '-C', workDir, name]);
  } else {
    // runner의 uid/gid 대신 root:root로 기록한다. root가 풀어도 runner uid 소유 파일이 생기지 않는다.
    const owner = process.platform === 'darwin'
      ? ['--uid', '0', '--gid', '0', '--uname', 'root', '--gname', 'root'] // bsdtar
      : ['--owner=0', '--group=0', '--numeric-owner']; // GNU tar
    process.env.COPYFILE_DISABLE = '1'; // macOS bsdtar가 ._ 메타데이터 파일을 넣지 않게 한다.
    run('tar', [...owner, '-czf', archive, '-C', workDir, name]);
  }
  rmSync(workDir, { recursive: true, force: true });
  writeChecksums(options.outDir, [basename(archive)]);
  console.log(`완료: ${archive}`);
}

main().catch((error) => fail(error instanceof Error ? error.message : String(error)));
