import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import App from "./App";
import "./App.css";

// 远程控制台入口。子任务 3 将替换为触控调音台（推子/Mute/电平表）；
// 当前为占位页，用于验证 rust-embed 打包与 HTTPS 分发链路。
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
