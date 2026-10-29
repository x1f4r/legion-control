// The files of a checkout that the repository's own checks read.
//
// That is every file git tracks and every new one it would track, so a check run before a commit
// sees what the commit would add. Ignored files are not the repository's: build outputs, above all,
// are full of the paths of the machine that built them. git is asked rather than the directory
// walked, because only git knows which is which.
import { spawnSync } from 'node:child_process';

/**
 * The checkout's files under `root` that match the git pathspecs in `patterns` (all of them when
 * there are none), relative to `root` with forward slashes, each once, in git's order. A file git
 * still tracks but the working tree no longer has is listed too; readers skip what they cannot read.
 */
export function checkoutFiles(root, patterns = []) {
  const listed = spawnSync('git', ['-C', root, 'ls-files', '-z', '--cached', '--others', '--exclude-standard', '--', ...patterns], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  if (listed.error || listed.status !== 0) {
    throw new Error(`git could not list the files of ${root}: ${(listed.stderr || listed.error?.message || `exit ${listed.status}`).trim()}`);
  }
  return [...new Set(listed.stdout.split('\0').filter(Boolean))];
}
