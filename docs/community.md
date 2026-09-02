# Community and support

Join the OxideLink community to get help, share profiles, and follow development.

## Links

- **Discord server:** https://discord.gg/oxidelink *(placeholder invite)*
- **GitHub repository:** https://github.com/jxoesneon/oxidelink

## Getting support

The fastest way to get help is to ask in the `#support` channel on Discord. Before posting:

1. Check the docs in the `docs/` folder.
2. Look at the [CHANGELOG.md](../CHANGELOG.md) to see if your issue was fixed in a newer release.
3. Include your OxideLink version, Windows version, and controller connection type (USB or Bluetooth).
4. Attach the relevant section of the app log or a crash report file if applicable.

## Reporting bugs

If you find a bug:

1. Search the GitHub Issues to see if it has already been reported.
2. If not, open a new issue and include:
   - A clear description of what happened.
   - Steps to reproduce.
   - Expected and actual behavior.
   - Your `profiles.json` or a minimal profile that triggers the issue, if relevant.
   - App logs (`%AppData%\OxideLink\logs`).

## Contributing

We welcome contributions! To get started:

1. Fork the repository on GitHub.
2. Create a feature branch (`git checkout -b feature/my-feature`).
3. Make your changes. Keep commits focused and write descriptive messages.
4. Run `cd src-tauri && cargo check && cargo test --lib`, then run `npm run build`, to verify the project still compiles and its Rust library tests pass.
5. Open a pull request against the `main` branch.

### Contribution guidelines

- Follow the existing Rust and JavaScript style.
- Add unit tests for new Rust logic where possible.
- Update user-facing docs in `docs/` and the `README.md` if your change affects behavior.
- Do not commit private keys, certificates, or personal `.bin` dumps.

## Feature requests

Feature requests are tracked as GitHub Discussions or Issues. Use the `#feature-requests` channel on Discord for informal brainstorming.

## Code of conduct

Be respectful, constructive, and inclusive. See [CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) for the full text. Harassment or discriminatory language will not be tolerated.
