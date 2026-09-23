import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";

import { App } from "./app/App";
import { AuthProvider } from "./app/AuthProvider";
import "highlight.js/styles/github-dark-dimmed.css";
import "./styles/index.css";

const root = document.getElementById("root");
if (!root) {
  throw new Error("缺少 #root 挂载节点");
}

createRoot(root).render(
  <StrictMode>
    <BrowserRouter>
      <AuthProvider>
        <App />
      </AuthProvider>
    </BrowserRouter>
  </StrictMode>,
);
