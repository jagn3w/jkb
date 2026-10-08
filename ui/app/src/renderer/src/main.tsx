import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "./styles/tokens.css";
import "@xterm/xterm/css/xterm.css";
import "./styles/app.css";
import "./styles/terminal.css";
import "./styles/design.css";
import "./styles/plan.css";
import "./styles/prompts.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";

const root = document.getElementById("root");
if (root === null) throw new Error("index.html has no #root");

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
