@echo off
rem EIR E2E FAKE CLAUDE CLI — see fake-claude.mjs. Never contacts a real
rem model, and the JSON it emits always carries "problems": [].
node "%~dp0fake-claude.mjs"
