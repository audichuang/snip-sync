// Timeline selection: click an anchor, Shift + click the other end, then
// turn the two rows into a CommitSelection. The backend re-checks
// contiguity (commits::select_range); this also refuses picks off the
// first-parent chain, which `base..tip` would silently turn into others.
import type { CommitSelection } from "../generated/CommitSelection";
import type { CommitSummary } from "../generated/CommitSummary";

export interface RangeEnds {
	anchor: string;
	end: string;
}

/** Next state after a click; Shift extends from the anchor. */
export function clickCommit(
	prev: RangeEnds | null,
	sha: string,
	shift: boolean,
): RangeEnds {
	return shift && prev
		? { anchor: prev.anchor, end: sha }
		: { anchor: sha, end: sha };
}

/**
 * The first-parent chain from the newer end down to the older one, newest
 * first. Empty when the older end is not on that chain (not contiguous).
 */
export function chainBetween(
	commits: CommitSummary[],
	ends: RangeEnds,
): string[] {
	const index = new Map(commits.map((c, i) => [c.sha, i]));
	const a = index.get(ends.anchor);
	const b = index.get(ends.end);
	if (a === undefined || b === undefined) return [];
	// `list_commits` is newest first: the smaller index is the tip.
	const tip = commits[Math.min(a, b)].sha;
	const oldest = commits[Math.max(a, b)].sha;
	const chain: string[] = [];
	let sha: string | undefined = tip;
	while (sha !== undefined) {
		chain.push(sha);
		if (sha === oldest) return chain;
		const at = index.get(sha);
		sha = at === undefined ? undefined : commits[at].parents[0];
	}
	return [];
}

export type RangeRequest =
	| { ok: true; selection: CommitSelection }
	| { ok: false; reason: "rootRangeUnsupported" }
	| { ok: false; reason: "discontinuous"; tip: string; oldest: string };

/**
 * `base..tip` with base = the older end's first parent. A range reaching the
 * root commit has no base; it is expressible only as "last n" from HEAD.
 */
export function toCommitSelection(
	commits: CommitSummary[],
	ends: RangeEnds,
): RangeRequest {
	const a = commits.findIndex((c) => c.sha === ends.anchor);
	const b = commits.findIndex((c) => c.sha === ends.end);
	const tipIndex = Math.min(a, b);
	const tip = commits[tipIndex];
	const oldest = commits[Math.max(a, b)];
	const chain = chainBetween(commits, ends);
	if (chain.length === 0) {
		return {
			ok: false,
			reason: "discontinuous",
			tip: tip.sha,
			oldest: oldest.sha,
		};
	}
	const base = oldest.parents[0];
	if (base !== undefined) {
		return { ok: true, selection: { kind: "range", base, tip: tip.sha } };
	}
	// `list_commits` starts at HEAD, so index 0 is HEAD.
	if (tipIndex === 0) {
		return { ok: true, selection: { kind: "last", n: chain.length } };
	}
	return { ok: false, reason: "rootRangeUnsupported" };
}
