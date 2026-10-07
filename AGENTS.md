# yonder agent notes

- Reply to the user in Chinese. Code, comments, commits in English.
- Source of truth for scope: docs/requirements.md (16 decisions). Record new decisions as docs/adr/NNNN-*.md.
- Relay must never see plaintext. Any feature needing content goes to host or client.
- Every layer is verified on three real hosts before moving on:
  - macOS: this Mac
  - Linux: the Linux test host
  - Windows (native, ConPTY): the Windows test host
- Relay deploy target: the maintainer's relay host behind nginx (`--behind-proxy`). Host names, addresses and access details live outside the repo. No secrets in repo.
- No AI attribution in commits.
