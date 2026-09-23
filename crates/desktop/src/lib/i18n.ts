import i18n from "i18next";
import en from "./locales/en.ts";
import zhHant from "./locales/zh-Hant.ts";

export type MessageKey = keyof typeof en;
/** The slice of i18next's `t` the pure helpers need. */
export type Translate = (
	key: MessageKey,
	params?: Record<string, unknown>,
) => string;

export const resources = {
	en: { translation: en },
	"zh-Hant": { translation: zhHant },
};

export type Language = keyof typeof resources;

const STORAGE_KEY = "language";

/** Any Chinese locale gets Traditional Chinese; everything else English. */
export function pickLanguage(
	stored: string | null,
	navigatorLanguage: string,
): Language {
	if (stored === "en" || stored === "zh-Hant") return stored;
	return navigatorLanguage.toLowerCase().startsWith("zh") ? "zh-Hant" : "en";
}

function storedLanguage(): string | null {
	try {
		return localStorage.getItem(STORAGE_KEY);
	} catch {
		return null;
	}
}

export function setLanguage(lng: Language): void {
	try {
		localStorage.setItem(STORAGE_KEY, lng);
	} catch {
		// Remembering the choice is a convenience only.
	}
	void i18n.changeLanguage(lng);
}

/** Initialises the shared instance for the webview. */
export function initI18n() {
	return i18n.init({
		resources,
		lng: pickLanguage(storedLanguage(), navigator.language),
		fallbackLng: "en",
		// Some keys are backend error texts that contain dots and colons.
		keySeparator: false,
		nsSeparator: false,
		interpolation: { escapeValue: false },
	});
}

/** Backend errors we know are shown translated; anything else verbatim. */
export function errorText(t: Translate, error: unknown): string {
	const message = String(error);
	const key = `err.${message}`;
	return key in en ? t(key as MessageKey) : message;
}

export default i18n;
