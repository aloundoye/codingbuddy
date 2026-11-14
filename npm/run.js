#!/usr/bin/env node
"use strict";

const { spawn } = require("child_process");
const path = require("path");

const executable = process.platform === "win32" ? "codingbuddy.exe" : "codingbuddy";
const child = spawn(path.join(__dirname, "bin", executable), process.argv.slice(2), {
  stdio: "inherit",
});

child.on("error", (error) => {
  console.error(`Could not launch CodingBuddy: ${error.message}`);
  process.exitCode = 1;
});
child.on("exit", (code) => {
  process.exitCode = code ?? 1;
});
