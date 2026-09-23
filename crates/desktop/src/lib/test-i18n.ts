// Test helper: a real i18next instance over the app's resources, so tests
// check the exact rendered strings.
import { createInstance } from "i18next";
import { resources, type Language, type Translate } from "./i18n.ts";

export async function translator(lng: Language = "en"): Promise<Translate> {
	const i18n = createInstance();
	await i18n.init({
		resources,
		lng,
		keySeparator: false,
		nsSeparator: false,
		interpolation: { escapeValue: false },
	});
	return (key, params) => i18n.t(key, params);
}
