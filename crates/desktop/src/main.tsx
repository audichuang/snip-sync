import { attachConsole } from "@tauri-apps/plugin-log";
import ReactDOM from "react-dom/client";
import { initReactI18next } from "react-i18next";
import App from "./App";
import i18n, { initI18n } from "./lib/i18n";
import "./index.css";

attachConsole().catch((error: unknown) => {
	console.error("Failed to attach Tauri log stream:", error);
});

i18n.use(initReactI18next);
initI18n()
	.catch((error: unknown) => {
		console.error("Failed to initialise i18n:", error);
	})
	.finally(() => {
		// No <StrictMode>: its double-mounted ref callback leaves
		// @pierre/diffs' PatchDiff with an empty <pre> (measured on 1.4.3).
		ReactDOM.createRoot(
			document.getElementById("root") as HTMLElement,
		).render(<App />);
	});
