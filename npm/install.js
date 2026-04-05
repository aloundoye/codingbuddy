#!/usr/bin/env node
"use strict";

const { execSync } = require("child_process");
const fs = require("fs");
const https = require("https");
const os = require("os");
const path = require("path");

const VERSION = require("./package.json").version;
const REPO = "alassanendoye/codingbuddy";

function getPlatformTarget() {
  const platform = os.platform();
  const arch = os.arch();

  const targets = {
    "darwin-x64": "x86_64-apple-darwin",
    "darwin-arm64": "aarch64-apple-darwin",
    "linux-x64": "x86_64-unknown-linux-gnu",
    "linux-arm64": "aarch64-unknown-linux-gnu",
    "win32-x64": "x86_64-pc-windows-msvc",
  };

  const key = `${platform}-${arch}`;
  const target = targets[key];
  if (!target) {
    console.error(`Unsupported platform: ${key}`);
    process.exit(1);
  }
  return target;
}

function downloadFile(url, dest) {
  return new Promise((resolve, reject) => {
    const follow = (url) => {
      https
        .get(url, (res) => {
          if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
            follow(res.headers.location);
            return;
          }
          if (res.statusCode !== 200) {
            reject(new Error(`Download failed: HTTP ${res.statusCode}`));
            return;
          }
          const file = fs.createWriteStream(dest);
          res.pipe(file);
          file.on("finish", () => {
            file.close();
            resolve();
          });
        })
        .on("error", reject);
    };
    follow(url);
  });
}

async function main() {
  const target = getPlatformTarget();
  const ext = os.platform() === "win32" ? ".zip" : ".tar.gz";
  const asset = `codingbuddy-v${VERSION}-${target}${ext}`;
  const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${asset}`;

  const binDir = path.join(__dirname, "bin");
  fs.mkdirSync(binDir, { recursive: true });

  const tmpFile = path.join(os.tmpdir(), asset);
  console.log(`Downloading codingbuddy v${VERSION} for ${target}...`);

  try {
    await downloadFile(url, tmpFile);
  } catch (err) {
    console.error(`Failed to download: ${err.message}`);
    console.error(`URL: ${url}`);
    console.error(
      "You can build from source: cargo build --release --bin codingbuddy",
    );
    process.exit(1);
  }

  const binName = os.platform() === "win32" ? "codingbuddy.exe" : "codingbuddy";
  const binPath = path.join(binDir, binName);

  if (ext === ".tar.gz") {
    execSync(`tar xzf "${tmpFile}" -C "${binDir}" --strip-components=1`, {
      stdio: "inherit",
    });
  } else {
    execSync(`powershell -Command "Expand-Archive -Path '${tmpFile}' -DestinationPath '${binDir}'"`, {
      stdio: "inherit",
    });
  }

  if (os.platform() !== "win32") {
    fs.chmodSync(binPath, 0o755);
  }

  fs.unlinkSync(tmpFile);
  console.log(`Installed codingbuddy to ${binPath}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
