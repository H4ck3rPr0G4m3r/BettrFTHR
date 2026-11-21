import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "./styles.css";
import { App } from "./App";
import { useApp } from "./lib/store";

// Dev only: lets tooling drive the UI (e.g. switch views) without clicking.
if (import.meta.env.DEV) Object.assign(window, { __app: useApp });

// No browser context menu (reload, inspect) in the shipped app.
if (!import.meta.env.DEV) window.addEventListener("contextmenu", (e) => e.preventDefault());

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
