## We want these boots to be as FAST as possible. I need these microvms to be as small as possible. That's part of the point of this library

---

Wait... we rebuilt this refactor with the runtime/decoration SDK that was previously provided by `mvmforge` (at `../mvmforge`), did that not get ported over? That's a bug. We MUST have this SDK pulled in.

If we put the sign and hostkey in the manifest, can't that be corrupted? If so, wouldn't this open a security area?

I think cloud-hypervisor (option 1) is the best backend plan. Do we currently have backends as a trait?

We want to keep firecracker

What do you think about a portable execution form factor?
