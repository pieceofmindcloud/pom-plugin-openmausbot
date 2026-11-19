# OpenMausBot

Open **OpenMausBot** in the POM sidebar. The first visit installs it by itself, behind POM's installer: a progress bar with the current step (downloading the Node.js runtime, installing OpenMausBot, connecting to POM's models). When it finishes, the OpenMausBot chat app opens edge to edge. If something fails, the error is shown with **Try again**, which retries without losing bots or chats. Later visits open the app directly.

## What is installed

- Node.js 24 (an official nodejs.org build, checked against its SHA-256) and the official `openmausbot` npm package at a pinned version, installed with `npm ci` from a lockfile shipped inside the plugin, so npm verifies every package. Nothing has to exist on the machine beforehand.
- Program files live next to POM's plugin database, in `openmausbot/` (`install.log` and `server.log` are there too). Bots, chats and settings live in the POM workspace under `.openmausbot/`.

## Models

The plugin adds an engine named **POM** to OpenMausBot (an OpenAI-compatible instance in its `config.json`) that points at this node's own `/v1`, and it picks the first deployed model as the default. Choose it in a bot's model picker or during OpenMausBot's onboarding ("Other ways to connect"). The node's gateway key is handed to the server in its environment and is never written to disk. Other engines (Claude, ChatGPT/Codex, OpenCode and the rest) stay available in OpenMausBot's own settings.

## Access and safety

OpenMausBot is local to this node. The plugin presents everyone who opens its screen in POM as OpenMausBot's owner, so an authenticated POM operator who can open this plugin can manage all of it, including bots that run commands on this machine and any stored provider keys. Do not grant POM UI access to untrusted operators. OpenMausBot's own port listens on loopback only, and browsers that reach it directly (outside POM) still have to pair.

Closing the plugin from the menu stops the server and everything it started (bot engines, shells, browsers).

## Phone access

By default OpenMausBot is reachable only through POM. To use the OpenMausBot phone app, open **Phone access** (bottom right of the screen) and choose:

- **MausBot tunnel**: a free public HTTPS address from OpenMausBot. Sign in to an OpenMausBot account right there (an 8-digit code arrives by email). Traffic passes through OpenMausBot's servers.
- **Tailscale**: only devices on your tailnet. Tailscale must be installed and signed in where the node runs (inside the container, in Docker) and on the phone.
- **Local network** (can be combined with either): not encrypted. In Docker with bridge networking the QR carries the container address, which phones cannot reach.

Saving restarts OpenMausBot on a fixed port (8799 by default). Then pair the phone in OpenMausBot's **Settings, Phone & devices** and scan the QR inside the OpenMausBot app. Anyone who pairs reaches bots that run commands on this machine: pair only your own devices.

