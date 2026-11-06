import { createHash } from 'node:crypto';
import { readdir, readFile } from 'node:fs/promises';
import { join, relative } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

/** Files the runner itself manages; everything else in the project is the Host's work area. */
const managed = (path) => ['config.json', 'mcp.json'].includes(path) || path.startsWith('.devlink-runtime/') || path === '.git' || path.startsWith('.git/');

/** Digest of every non-managed file path and content under `folder`, independent of which tool wrote it. */
export async function projectSnapshot(folder) {
  const entries = [];
  const walk = async (directory) => {
    for (const item of await readdir(directory, { withFileTypes: true })) {
      const path = join(directory, item.name), name = relative(folder, path).replaceAll('\\', '/');
      if (managed(name)) continue;
      if (item.isDirectory()) await walk(path);
      else entries.push(name + '\0' + createHash('sha256').update(await readFile(path)).digest('hex'));
    }
  };
  await walk(folder);
  return createHash('sha256').update(entries.sort().join('\n')).digest('hex');
}

/**
 * AC3 watcher. Each sample takes the file snapshot first and then reads the receiver's central
 * task states, so a snapshot paired with "every received task still PENDING" was necessarily
 * taken before acceptance: any difference from the initial snapshot is a pre-acceptance change,
 * whether it came from an edit tool or a shell write. A change made between the last PENDING
 * sample and the acceptance (one interval) is not visible here; runners pair this watcher with
 * a transcript check that shell commands before the acceptance were read-only.
 */
export function watchUnchangedWhilePending({ folder, initial, receivedStatuses, intervalMs = 250 }) {
  // intervalMs is the pause between samples; each sample also takes a snapshot and a central query.
  let stopped = false;
  const result = { samples: 0, pendingSamples: 0, taskPendingSamples: 0, violations: 0, errors: 0, acceptedObserved: false };
  const loop = (async () => {
    while (!stopped) {
      try {
        // A Host editing after acceptance may create and remove a temporary file mid-walk; retry that sample once.
        const snapshot = await projectSnapshot(folder).catch((error) => {
          if (error?.code === 'ENOENT') return projectSnapshot(folder);
          throw error;
        });
        const statuses = await receivedStatuses();
        result.samples += 1;
        if (statuses.every((status) => status === 'PENDING')) {
          result.pendingSamples += 1;
          if (statuses.length > 0) result.taskPendingSamples += 1;
          if (snapshot !== initial) result.violations += 1;
        } else result.acceptedObserved = true;
      } catch { result.errors += 1; }
      await delay(intervalMs);
    }
  })();
  return {
    current: () => ({ ...result }),
    async stop() { stopped = true; await loop; return { ...result }; },
  };
}

/** Pre-acceptance evidence is usable only when it saw the task PENDING, saw it accepted, and never failed to sample. */
export function assertWatchEvidence(watched) {
  if (watched.errors !== 0) throw new Error('Pre-acceptance file watch had sampling errors');
  if (!(watched.taskPendingSamples > 0 && watched.acceptedObserved)) throw new Error('Pre-acceptance file watch did not see the task both PENDING and accepted');
  if (watched.violations !== 0) throw new Error('Project files changed while the incoming task was still PENDING');
}
