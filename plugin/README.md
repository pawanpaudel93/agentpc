# agentpc plugin for Claude

Gives Claude instant, resettable Windows and Ubuntu desktops on your Mac. Claude can create a VM
in seconds, click, type, take screenshots and run commands in it, then reset it to a
clean install.

The plugin bundles:

- the `agentpc` MCP server (VM lifecycle, screenshots, shell and GUI control), and
- the `agentpc` skill, which teaches Claude when and how to use the VMs.

## Requirements

- An Apple Silicon Mac (M1 or later) running a recent macOS (one QEMU supports).
- The `agentpc` binary on your `PATH`:

  ```sh
  curl -fsSL https://agentpc.pawanpaudel.com.np/install.sh | sh
  ```

## Install

In Claude Code:

```text
/plugin marketplace add pawanpaudel93/agentpc
/plugin install agentpc@agentpc
```

Then ask Claude something like *"Create an Ubuntu VM and check that my install script works on
a clean machine."*

The first Ubuntu VM downloads the Ubuntu image (~1.2 GB). Windows images can't be
redistributed, so build yours once with `agentpc image build windows`, which downloads the
official ISO from Microsoft. Other versions work side by side, e.g. `ubuntu-22.04` or
`windows-11-23h2`. For x86_64 Linux programs and amd64 containers, use `ubuntu-x86apps`
(translated by FEX); on Windows, x64 apps run through Prism.

## More

See the [agentpc repository](https://github.com/pawanpaudel93/agentpc) for documentation.

## License

[MIT](LICENSE)
