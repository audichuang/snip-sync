import type { GitLogEntry } from "@tomplum/react-git-log";
import type { RepositoryHistory } from "../generated/RepositoryHistory";

/** Assign each first-parent chain to a lane; merge parents keep their own lane. */
export function graphEntries(history: RepositoryHistory): GitLogEntry[] {
	const lanes = new Map<string, string>();
	for (const ref of history.refs) {
		if (!lanes.has(ref.sha)) lanes.set(ref.sha, ref.name);
	}
	if (history.head && !lanes.has(history.head))
		lanes.set(history.head, "HEAD");
	return history.commits.map((c) => {
		const branch = lanes.get(c.sha) ?? c.sha.slice(0, 8);
		const parent = c.parents[0];
		if (parent && !lanes.has(parent)) lanes.set(parent, branch);
		return {
			hash: c.sha,
			parents: c.parents,
			branch,
			message: c.subject,
			committerDate: c.authorDate,
			authorDate: c.authorDate,
			author: { name: c.authorName, email: c.authorEmail },
		};
	});
}
export const shortRef = (ref: string) =>
	ref.replace(/^refs\/(heads|remotes|tags)\//u, "");
