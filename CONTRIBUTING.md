# Contributing

Thanks for helping improve `compose-to-desktop-copier`.

## Before opening a pull request

- Explain the problem and the proposed change.
- Keep template changes compatible with Copier updates.
- Update the README or template tests when behavior changes.
- Do not commit secrets, local `.env` files, build output, or generated test projects.

Run the template checks locally:

```text
bash scripts/test-template.sh
```

The checks require Copier 9, Rust, and the Linux Tauri/WebView development packages when run on Linux. Pull requests should leave the working tree clean and pass the GitHub Actions workflow.

## Pull requests

Keep each pull request focused. Include a short description of the user-visible effect and note any migration or compatibility impact. Changes to the Compose contract or Copier questions should update `CHANGELOG.md`.
