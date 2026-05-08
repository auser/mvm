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

---

I'm coming from a previous iteration of `mvm` that's in `../mvm`, but this time I'm trying to use microsandbox as a builder.

The very high-level goal here is to use microsandbox to build a microvm as we did before, but this time using microcontainer which runs on man different hosts.

For execution, we can use microcontainer to execute the microvm, but want to prefer firecracker. Can you also do a gap analysis of what we have and this new, very simplified version and try to bring this one up closer to the same/similar functionality so that we can execute these microvms (that we're building with nix) -- the promise of this project is the most secure microvm orchestration -- with `mvmd` (located at `../mvmd`) -- and combining microvms with nix. The output of the build pipeline should be a complete microvm built using nix so that we have zero drift.

In addition, we want to provide an executable development mode where a user can interact with their microvm but NEVER allow that in production. For more detail, I've brought over the `specs/` directory for more information

We want to make sure we keep the BEST possible DX which the previous iteration included in a crate called `mvm-sdk` which was transformed from `../mvmforge` to be embedded.

One of the benefits of using microcontainer is that it uses libkrun, which is natively supported in macos and linux and I _think_ windows.

`mvmd` is the orchestrator -- also in this simplified version we have zero encryption, which we absolutely must have. We also need a way to host secrets.

We need to bring over volume mounts, snapshots, metrics, logs, DNS, events, etc. as is implemented in the previous iteration in `mvm`

We also need tests EVERYWHERE as we did in our previous iteration (we have zero tests in this simplified version)
