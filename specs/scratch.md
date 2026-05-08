We want these boots to be as FAST as possible. I need these microvms to be as small as possible. That's part of the point of this library

---

When you're done, make a git commit with all the changes in all the files. After you merge to main, checkout a `feat/` branch and plan the next sprint in @specs/SPRINT.md

---

Keep @specs/SPRINT.md up-to-date, save this plan in `specs/plans`, and checkout a new `feat/` branch to do this work on

---

I found this which says it offers a way to run commands in a microvm, which seems kind of similar to what we're doing. Can we learn anything from it?

https://github.com/braiins/llm-jail.git

---

Is that `nix/flake.nix` really large? Should it be smaller?

---

Go ahead after reviewing the code as it has changed significantly since writing this plan.

---

Can you review the template commands so that we can create a flake and build off that flake a template? I'm not sure the create template makes much sense -- I think the templates should be built off flakes everywhere. Would that be a better DX?

---

Remember security and safety are the most important features of this project -- we have to make sure we pass any security checks

And we kick off 24 jobs? I think that's a lot of jobs... why do we have 24 queued in github actions?

---

Can we work on this in a worktree as specified in [@AGENTS.md](file:///Users/auser/work/personal/microvm/kv/mvm/AGENTS.md)
