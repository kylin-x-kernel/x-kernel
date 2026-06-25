# X-Kernel Adaptation Draft

## X-Kernel Design Goal

## Adopted Linux Semantics

## Deliberately Dropped Semantics

## Architecture

## Components

## Crate Plan

### For each crate

- Crate name
- Role in subsystem
- Why this boundary exists
- Direct dependencies
- Explicit non-responsibilities

## Data Structures

### For each key structure

- Name
- Owning crate
- Ownership meaning
- Core fields
- Field intent
- Lifecycle
- Concurrency notes

## Interfaces

### For each core interface

- Name
- Owning crate
- Kind: trait / struct API / free function / internal protocol
- Inputs
- Outputs
- Error model
- Blocking/sleeping rules
- Caller obligations
- Callee guarantees
- Why this interface boundary exists

## Lifetime and Ownership Model

## Locking Model

## Failure Model

## Staged Rollout

## Open Questions
