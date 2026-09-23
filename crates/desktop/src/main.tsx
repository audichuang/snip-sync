import { attachConsole } from "@tauri-apps/plugin-log";
import * as React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";

attachConsole().catch((error: unknown) => {
	console.error("Failed to attach Tauri log stream:", error);
});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
	<React.StrictMode>
		<App />
	</React.StrictMode>,
);
