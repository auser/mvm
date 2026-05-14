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

---

Our SDK model supports two modes:

- Decorator
- Runtime

The decorator syntax is similar to modal: https://modal.com/docs/guide -- it has a decorator around an entrypoint that defines all we need to launch a microvm and compiles down to a nix template. This should/might have to interact with both mvm and mvmd, but I want it in here and maybe an extension in mvmd for resources (like cpu, disk, memory, network, etc)

The runtime syntax is similar to e2b: https://www.e2b.dev/docs/template/quickstart -- in development mode it should be able to interact with a real running microvm. This is only in dev mode. When interacting with a microvm in production mode it cannot interact directly with the microvm, but it should also compile down to the IR.

Both of these syntaxes are important to support.

---

I would prefer to keep all _sdk_ work in a crate called `mvm-sdk` rather than multiple crates. For example, this design means `mvm-ir`'s functionality needs to be nested under this `mvm-sdk` crate.

Both SDKs should compile down to a nix template so a user can use familiar language to build their nix template for their own microvm.

# Decorator syntax

The compile doesn't mean shell to `python app.py` on the host, but it provides an entrypoint to the microvm of `app.py` -- that's where the microvm should start. In the developer mode the entrypoint is a shell, but in this case the entrypoint is the python function decorated. Also the decorated function should be `@mvm.app()`, not `@mvm.func()` to be more descriptive. `@mvm.func()` feels like a library. Every app should correspond to a microvm

This decoration syntax should use a python template, build one, and be able to pull in the dependencies of the app, either local or remote as sepcified.

# Runtime syntax

The runtime syntax should also be able to compile into a nix template.

For the dev/prod boundary, the library should decide what mode we're in based on the command a user is running.

Decisions

A: The runtime prod-mode behavior only interacts with a running microvm, otherwise it should compile into an `ExecutionPlan` in dev modes. In prod, the commands need to compile into a nix template.

B: Yes, rename it to `MVM_`. `MVMFORGE_` is deprecated

C: Just keep `mvm`, not `mvmforge`

D: Continue with the ADR numbering from `mvm`

---

I think that a user should be able to use a flag or environment variable to determine what mode they are in, however it should default to the command a user is running.

In the decorator syntax, those resources and network functions should map to `mvmd`, right, but the other side of it maps directly to `mvm`.

We'll also need to add an `mvmctl deploy` command which compiles and calls a stub input which will eventually ship the compiled archive to a remote `mvmd` server. For now, just add the stub.

I think the `addon` is like a template where a user can something like this where the `database` and the `service` are addons that make it easy to extend the workload.

`app.database({ name: "users" }).service("authx").app()`

We need to support python and typescript utilizing the ir.

Decisions:

E: No, just use `.app()`. No need to support back-compat
F: I think allow a small set of `mvm` and `mvmd` helper calls
G: I think `hello-app` is right

---

Could we extend the input with a file, e.g.: `--input @filename` or `--input-file @filename` or accept stdin e.g. `mvmctl invoke hello-app < filename`

For the addons, for customization, in addition to the fluent syntax could we also have them generated like:

```python
db = mvm.addons.database()
app = mvm.app("my-app").database(db)...
```

Can we also have a directory to package with the shipped apps to include as a mounted directory in the microvm? I mean we might have a typescript app that has a `src/` and `lib/` or `packages/` directory in the cwd, I want to make sure those get added to the microvm.

Along those same lines, we should have python pip install and npm install when setting up a microvm. Should that pip/npm installation happen on the host and mount as a volume? I kind of think that should run on the microvm into a volume that can be later mounted on a running microvm. Dependencies need some extra work, I think. Can you propose how we should work with dependencies?

Explore how e2b does this: https://www.e2b.dev/docs/template/ in particular their lifecycle hooks which make sense for us to support -- lifecycle hooks and within templates, etc. and base images

---

On second though about the `launch.json`, would that be specific to a tenant's launch? Would it make sense to keep it separate? I'm curious about your recommendation here.

For the `mvmctl invoke <template> --input name='ari'`, would it make sense for us to use the function name as well or does the `app` decoration take care of that for us? What do you think?

For the hooks, should we have `before_` and `after_` instead of `on_` to be more specific? Also I think addons should have their own hooks so when an addon is added, the addon/template defines their own hooks which get stacked with others so the user doesn't always have to specify it.

Also for both the decorator and the runtime SDK, the user should be able to set environment variables.

We do NOT NEED TO SUPPORT BACKWARDS COMPATIBILITY. THIS IS THE FIRST VERSION.

Do this work in a worktree. Save the plan entirely in `specs/plans` and keep `specs/SPRINT.md` up-to-date
