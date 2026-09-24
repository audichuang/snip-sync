import process from "node:process";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig({
	plugins: [react(), tailwindcss()],
	// Keep Rust errors visible and use the fixed port Tauri expects.
	clearScreen: false,
	server: {
		port: 1420,
		strictPort: true,
		host: host || false,
		...(host ? { hmr: { protocol: "ws", host, port: 1421 } } : {}),
		watch: { ignored: ["**/src-tauri/**"] },
	},
});
