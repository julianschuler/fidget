# Fidget
Fidget is experimental infrastructure for complex closed-form implicit surfaces.

It is **not ready for public use**, but is published to
[crates.io](https://crates.io) to claim the package name.

(As such, I'd appreciate if people didn't share it to news aggregators or post
about it on Twitter.  If you feel an overwhelming urge to talk about it, feel
free to [reach out directly](https://mattkeeter.com/about))

That being said, it already includes a bunch of functionality:

- Manipulation and deduplication of math expressions
- Conversion from graphs into straight-line code ("tapes") for evaluation
- Tape simplification, based on interval evaluation results
- A _very fast_ JIT compiler, with hand-written AArch64 routines for
    - Point-wise evaluation (`f32`)
    - Interval evaluation (`[lower, upper]`)
    - SIMD evaluation (`f32 x 4`)
    - Gradient evaluation (partial derivatives with respect to x, y, and z)

These building blocks are used in an implementation of bitmap rendering.

## Crate features
The project is based on the `fidget` crate, with three relevant features

- `render` builds `fidget::render`, which includes functions to render 2D and
  3D images.
- `rhai` builds [Rhai](https://rhai.rs/) bindings
- `jit` builds the JIT compiler

By default, all of these features are enabled.

## Platforms
At the moment, only macOS (AArch64) is fully supported.

Disabling the `jit` feature should allow for cross-platform rendering
(albeit without the JIT compiler), but this is untested.
