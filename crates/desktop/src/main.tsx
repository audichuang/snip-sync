import { attachConsole } from "@tauri-apps/plugin-log";
import ReactDOM from "react-dom/client";
import { initReactI18next } from "react-i18next";
import App from "./App";
import i18n, { initI18n } from "./lib/i18n";
import "./index.css";

i18n.use(initReactI18next);
// Neither failure stops the app: without the log stream the webview still
// logs locally, and i18next falls back to the keys.
const [logStream, translations] = await Promise.allSettled([
	attachConsole(),
	initI18n(),
]);
if (logStream.status === "rejected") {
	console.error("Failed to attach Tauri log stream:", logStream.reason);
}
if (translations.status === "rejected") {
	console.error("Failed to initialise i18n:", translations.reason);
}

// No <StrictMode>: its double-mounted ref callback leaves
// @pierre/diffs' PatchDiff with an empty <pre> (measured on 1.4.3).
ReactDOM.createRoot(document.querySelector("#root") as HTMLElement).render(
	<App />,
);
