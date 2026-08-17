# M1f manager package safety contract

## Goal

Give the manager ownership of a user-local Plasma package transaction without
modifying the live system package or requiring a Plasma restart from the
manager itself.

## Contract

- Validate `metadata.json`, the Plasma wallpaper package structure, package ID,
  and the required QML entry point before copying anything.
- Reject symlinks and non-regular package files.
- Copy into a sibling `.new` directory, then retain the previous package as
  `.previous` and promote the new directory as one bounded transaction.
- Enter safe mode by renaming the user-local package to `.disabled`; leave safe
  mode by restoring that directory.
- Keep the package root configurable for tests and development installs.
- Report every failure in the manager UI. A failed install must leave the
  previous package untouched.

## Explicit boundary

This slice does not edit Plasma configuration, restart `plasmashell`, or claim
that applying a wallpaper is complete. The package is staged and recoverable;
the live Plasma PID-survival test remains an explicitly authorized gate.

## Provenance

The transaction and safe-mode implementation is original. It uses Qt's file,
directory, JSON, and QObject APIs. KDE's Plasma wallpaper package shape is a
public compatibility target documented in `THIRD_PARTY.yml`; no KDE or
upstream wallpaper-engine implementation was copied.
