# ESP-Claw Web 模拟器

用于在浏览器中运行独立的 Lua/LVGL App。页面根据 URL 中的 `app` 参数加载 `launcher.json` 和 App 文件，并根据 Lua 模块使用情况推导外设。

## 本地运行

依赖 Docker、Node.js 和 pnpm。在仓库根目录执行：

```bash
./pages/simulator/run-local.sh
```

浏览器访问：

```text
http://localhost:5173/?repo=skills-lab&ref=main&app=apps/flappybird/launcher.json
```

远程使用 VS Code 时转发 `5173` 端口。按 `Ctrl+C` 停止服务；编译缓存、运行时文件、前端依赖和 Docker 镜像会保留，以便下次增量构建。

彻底清理模拟器依赖、构建输出和 Docker 镜像：

```bash
./pages/simulator/run-local.sh --clean
```

## App 数据约定

`apps-data.json` 中每个 App 必须包含 `id` 和相对 App 目录的 `files` 数组。`VITE_SKILLS_LAB_WEB_BASE` 如需覆盖，必须指向 Skills Lab 站点根地址，例如：

```text
VITE_SKILLS_LAB_WEB_BASE=https://skills-lab.esp-claw.com
```

WASM 运行时实现和原生 Emscripten 构建方式见 [`tools/lua_lvgl_web_sim`](../../tools/lua_lvgl_web_sim/README.md)。
