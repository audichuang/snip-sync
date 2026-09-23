// Drives the pinned TS reference (.ts-ref) from the cross-tool tests.
//
//   node --experimental-strip-types scripts/ts-ref.mjs <ts-ref-dir> files <root> <path>...
//   node --experimental-strip-types scripts/ts-ref.mjs <ts-ref-dir> commit <root> <sha>
//   node --experimental-strip-types scripts/ts-ref.mjs <ts-ref-dir> restore <root>   (payload on stdin)
//
// `files` and `commit` print the payload to stdout; `restore` prints the
// execution result as JSON. All runs use the TS default settings.

import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const [tsRef, cmd, root, ...rest] = process.argv.slice(2);
if (!tsRef || !cmd || !root) {
  process.stderr.write('usage: ts-ref.mjs <ts-ref-dir> files|commit|restore <root> ...\n');
  process.exit(2);
}
const srcUrl = pathToFileURL(path.resolve(tsRef, 'src') + '/').href;

// The TS sources import siblings as './x.js'; the files on disk are './x.ts'.
registerHooks({
  resolve(specifier, context, next) {
    if (context.parentURL?.startsWith(srcUrl) && specifier.startsWith('.') && specifier.endsWith('.js')) {
      return next(specifier.slice(0, -3) + '.ts', context);
    }
    return next(specifier, context);
  }
});

const load = name => import(new URL(`${name}.ts`, srcUrl).href);
const { defaultSettings } = await load('settings');

function git(args, encoding = 'utf8') {
  return execFileSync('git', ['-C', root, ...args], { encoding, maxBuffer: 1 << 30 });
}

// A HistoryRepo backed by the git CLI, shaped like the VS Code git API's.
const repo = {
  rootUri: { fsPath: root },
  buffer: async (ref, relative) => git(['show', `${ref}:${relative}`], 'buffer'),
  async diffBetweenWithStats(ref1, ref2) {
    const fields = git(['diff', '-z', '--raw', '--no-abbrev', '-M', ref1, ref2]).split('\0');
    const changes = [];
    for (let i = 0; i + 1 < fields.length && fields[i]; ) {
      const status = fields[i].split(' ').pop().charAt(0);
      const renamed = status === 'R' || status === 'C';
      const oldPath = fields[i + 1];
      const newPath = renamed ? fields[i + 2] : oldPath;
      i += renamed ? 3 : 2;
      const uri = { fsPath: path.join(root, newPath) };
      changes.push(renamed
        ? { uri, originalUri: { fsPath: path.join(root, oldPath) }, renameUri: uri, status }
        : { uri, status });
    }
    return changes;
  }
};

if (cmd === 'files') {
  const { collectCopyFiles } = await load('copy');
  const result = await collectCopyFiles(root, rest, defaultSettings);
  process.stdout.write(result.payload);
} else if (cmd === 'commit') {
  const { listCommitFiles } = await load('gitHistory');
  const { buildGraphCopyPayload } = await load('graphCopy');
  const hash = git(['rev-parse', '--verify', `${rest[0]}^{commit}`]).trim();
  const parents = git(['rev-list', '--parents', '-n', '1', hash]).trim().split(' ').slice(1);
  const changes = await listCommitFiles(repo, { hash, message: '', parents });
  const files = changes.map(change => ({
    repoRootFsPath: root,
    relativePath: path.relative(root, (change.renameUri ?? change.uri).fsPath).split(path.sep).join('/'),
    status: change.status
  }));
  const deps = { resolveRepo: () => repo, workspaceRoots: [root], settings: defaultSettings };
  const result = await buildGraphCopyPayload(deps, { hash, files });
  process.stdout.write(result.text);
} else if (cmd === 'restore') {
  const { parseClipboard } = await load('clipboardFormat');
  const { planRestore, executeRestorePlan } = await load('restore');
  const entries = parseClipboard(readFileSync(0, 'utf8'), defaultSettings.headerFormat);
  const plan = await planRestore(root, entries);
  const result = await executeRestorePlan(plan, { overwriteExisting: true, skipExisting: false });
  process.stdout.write(JSON.stringify(result));
} else {
  process.stderr.write(`unknown command: ${cmd}\n`);
  process.exit(2);
}
