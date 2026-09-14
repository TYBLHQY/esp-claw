# App Registry Package Spec

This document defines standalone Lua App packages shown by the System UI Launcher.

## Layout

```text
apps/
└── app_id/
    ├── launcher.json
    ├── scripts/
    │   └── main.lua
    └── assets/
        └── icon.jpg
```

- Built-in Apps live under `application/edge_agent/fatfs_image/system/apps/` or a board's `fatfs_image/apps/` SYSTEM overlay.
- Runtime Apps live under the DATA root's `apps/` directory.
- DATA Apps shadow same-id SYSTEM Apps.
- App and Skill ids use independent namespaces and lifecycles.
- Apps must not reference files inside Skill packages.

## `launcher.json`

```json
{
  "schema_version": 1,
  "id": "light_switch",
  "display_name": "Light",
  "entry": "scripts/main.lua",
  "icon": "assets/icon.jpg",
  "args": {},
  "order": 10,
  "visible": true
}
```

- `schema_version` must be `1`.
- `id` is required, uses 1–63 ASCII letters, digits, underscores, or hyphens, and must match the package directory.
- `entry` is required and must name an existing package-relative `.lua` file.
- `icon` is optional and accepts package-relative `.jpg` or `.jpeg`; a missing file uses the default icon.
- `display_name` defaults to `id`.
- `args` must be an object when present.
- `order` must be an integer when present and defaults to `0`.
- `visible` must be a boolean when present and defaults to `true`.
- Relative paths must not be absolute, contain `..`, or contain backslashes.
- Unknown fields are ignored for forward compatibility.
- An invalid package is logged and skipped without blocking other Apps.

The browser simulator obtains the complete package file list from the Skills Lab App index and infers peripherals from Lua module usage. Simulator-specific configuration must not be added to `launcher.json`.

## Runtime Management

- `app_registry_publish()` validates and publishes a complete App already written below the DATA App root.
- `app_registry_remove()` only removes DATA Apps; SYSTEM Apps are read-only.
- Removing a DATA override reveals a same-id SYSTEM App after registry reload.
- Registry callbacks run after the registry lock is released.
