/**
 * The backend's unified diff (`similar`) has only `--- a/x` / `+++ b/y`
 * headers. @pierre/diffs strips the a/ b/ prefixes, and tells a rename from
 * an edit, only for git-style patches, so add the `diff --git` line.
 */
export function withGitHeader(patch: string): string {
	const m = /^--- (a\/.*)\n\+\+\+ (b\/.*)$/m.exec(patch);
	return m && !patch.startsWith("diff --git") ? `diff --git ${m[1]} ${m[2]}\n${patch}` : patch;
}
