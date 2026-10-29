Scopewerk for Windows
=====================

The Hanmatek DOS1102 oscilloscope on your PC: live screen, controls, measurements, a two-channel
logic analyzer (CAN, I2C, SPI, UART), a command line tool and an MCP server for AI agents.
https://mneuhaus.github.io/scopewerk/

One-time driver setup
---------------------
Windows talks to the scope through the generic WinUSB driver:

1. Connect the square USB-B port on the back of the scope and switch it on.
2. Download Zadig from https://zadig.akeo.ie and start it.
3. Options > List All Devices, then pick "Oscilloscope" (USB ID 5345 1235).
4. Choose "WinUSB" as the driver and click "Replace Driver" (or "Install Driver").

Scopewerk tells you when the scope is still on another driver.

Running
-------
Scopewerk.exe            the app (Windows may ask once: "More info" > "Run anyway";
                         the download is not code-signed)
cli\scopewerk.exe        the command line tool: scopewerk status, capture, set, decode, ...
cli\scopewerk.exe mcp    the MCP server for Claude, Codex and other agents

Try it without a scope: set SCOPEWERK_DEMO=can (or i2c, uart) before starting Scopewerk.exe.

MIT license. Source: https://github.com/mneuhaus/scopewerk
