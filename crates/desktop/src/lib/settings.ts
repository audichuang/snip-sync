// Reads the few settings the frontend itself acts on, from the same store
// value the backend normalizes (commands::load_settings).

/** TS showCopyNotification: default true; only a real boolean overrides it. */
export function showCopyNotification(stored: unknown): boolean {
	const v =
		stored !== null && typeof stored === "object"
			? (stored as Record<string, unknown>).showCopyNotification
			: undefined;
	return typeof v === "boolean" ? v : true;
}
