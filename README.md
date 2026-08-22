# ralphus

`ralphus` is an agent orchestrator. It handles Task submission, dependencies,
stacked PRs, auto-rebasing, building, and other features.

`ralphus` comes with a web interface and also can run terminal-only. A common
workflow is to build and submit from
[claude-code](https://claude.com/product/claude-code) and review from
[claude-code](https://claude.com/product/claude-code) once all of your branches are ready.

## Task View
<img width="2539" height="1038" alt="Image" src="https://github.com/user-attachments/assets/90fbd411-713f-436b-bf29-ec215edc66e1" />

## Reviews View
<img width="2521" height="1310" alt="Image" src="https://github.com/user-attachments/assets/11893a96-b12e-41b1-9d7c-0a2fce6cbcbc" />


## Related Projects
- [claudectl](https://mercurialsolo.github.io/claudectl) - A terminal-only
  switchboard for Claude sessions. It's useful though the project's stated
  goals differ from `ralphus`. Where `claudectl` aims for control and
  preference, `ralphus` pursues mass-parallelism. It also lacks customizations
  needed for many production environments. A useful tool for
  small-tomedium-sized use-cases.

- [gastown](https://github.com/gastownhall/gastown) - A terminal and GUI tool
  that has gained mass popularity. For good reason, its goals align well with
  `ralphus` but its token-heavy setup and steep learning curve make it unideal
  for "just getting things done" and makes troubleshooting difficult when
  systems go wrong.

- [ralphy](https://github.com/michaelshimeles/ralphy) - An excellent, simple
  tool that does a lot of what `ralphus` offers. `ralphus` is heavily inspired
  by `ralphy` and was built to extend to it higher levels of parallelism.
