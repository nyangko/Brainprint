# The pinned C# semantic backend — and what #19 task 12 measured with it

```sh
./restore.sh
```

This directory exists because task 12 opens with a **backend boundary
spike**, not with an implementation. Nothing here is installed
automatically, and no production Brainprint code reads it yet: the spike
reached two conflicts that have to be decided before a production
backend can be built. What follows is the measured record.

## Candidate A — the official Roslyn language server

```text
package   Microsoft.CodeAnalysis.LanguageServer.<rid>   5.4.0-2.26179.14
feed      https://pkgs.dev.azure.com/azure-public/vside/_packaging/vs-impl/nuget/v3/index.json
runtime   .NET 10 (measured against SDK 10.0.102, runtime 10.0.2)
launch    <pkg>/content/LanguageServer/<rid>/Microsoft.CodeAnalysis.LanguageServer \
            --stdio --logLevel Warning --extensionLogDirectory <dir>
```

`--logLevel` and `--extensionLogDirectory` are **required** arguments.
There is no `--autoLoadProjects` in this version; projects are loaded
through the server's own notifications, `solution/open` and
`project/open`.

Not on nuget.org, and there is no `roslyn-language-server` tool package
there either -- both were probed and both are absent. The `vs-impl` feed
is where Microsoft actually publishes it.

### What the handshake says

```text
serverInfo         absent  -> version identity must come from the package, not the protocol
positionEncoding   absent  -> LSP default, UTF-16
textDocumentSync   { openClose: true, change: 2 }
callHierarchyProvider  ABSENT  -- this server does not implement call hierarchy
```

21 providers are advertised, including `definitionProvider`,
`referencesProvider`, `implementationProvider`, `typeDefinitionProvider`
and `documentSymbolProvider`.

### The load barrier exists, and it is observable

`workspace/projectInitializationComplete` is sent by the server when the
solution has finished loading. Measured at **~8.1s** for the four-project
fixture, one notification, deterministic. That is a real barrier and not
a sleep.

One process, ~260-295 MB RSS at steady state. The `BuildHost` child used
to evaluate projects does not survive the load.

### What it answers correctly

Measured on `fixtures/workspaces/csharp-semantic-spike`:

| case | answer |
|---|---|
| cross-project definition | `App` -> `Core` -> `Contracts` all resolve |
| partial member | `runner.Compute` -> `Runner.Part2.cs`, the part that declares it |
| overload `Parse("x")` | the `string` overload |
| overload `Parse(1)` | the `int` overload |
| generic method / generic type | `Convert<T>`, `Box<T>` resolve |
| using alias | `Alias = Contracts.Model` -> `Model` |
| extension method | `runner.Label()` -> `RunnerExtensions.Label`, not the same-named `UnrelatedExtensions.Label` |
| base clause / interface clause | `BaseRunner`, `IRunner` resolve cross-project |
| explicit interface implementation | `void IRunner.Run()` -> `IRunner.Run` |
| `implementation` of `BaseRunner.Run` | all three overrides, across two files |
| virtual call through a base-typed local | the **declaration** `BaseRunner.Run`, which is the honest answer |
| references | intra-file, cross-file and cross-project, including the test project |

Overload selection, extension-method selection and the same-name traps
all pass **without** parsing hover prose. Candidate B was therefore never
built: the public boundary proves them.

### Conflict 1 -- the edit barrier needs a document overlay

```text
edit on disk + workspace/didChangeWatchedFiles  -> line 6, 6, 6   (STALE)
then textDocument/didOpen with the same bytes   -> line 8, 8, 8   (CURRENT)
```

`didChangeWatchedFiles` alone does **not** make the server current for a
`.cs` edit -- measured three times in a row, deterministically stale.
A `textDocument/didOpen` carrying the current Resource bytes does, also
deterministically, with no settle delay.

This is workable: it is the Brainprint-owned transport overlay the task
permits, carrying the same bytes the Workspace indexed. Recorded here
because it is the opposite of the TypeScript and Svelte tiers, both of
which were measured correct on watched files alone.

### Conflict 2 -- project loading executes arbitrary project code

A `.csproj` with

```xml
<Target Name="BrainprintMarker" BeforeTargets="CoreCompile;Build;ResolveReferences;GetTargetFrameworks">
  <WriteLinesToFile File="../../MSBUILD-TARGET-RAN" Lines="ran" Overwrite="true" />
</Target>
```

**ran during project load**: the marker file existed afterwards. Project
loading also created `obj/` inside the fixture.

There is no supported switch to stop it. The CLI has no option for it,
and it is not incidental: a design-time build is *how* Roslyn obtains a
project's compiler command line, so Candidate B (`MSBuildWorkspace`)
would execute the same targets for the same reason. Global properties can
be set; a project's own `<Target BeforeTargets=…>` cannot be suppressed.

This collides with Brainprint's locked trust principle. Task 11 was able
to honour it for Svelte because that server exposes
`initializationOptions.isTrusted`. Roslyn exposes no equivalent.

### External identity is derivable, from a header rather than a path

A framework target resolves into a decompiled `MetadataAsSource` file
whose path is a content hash and therefore useless as identity. The file
itself carries the identity:

```text
#region <assembly>  System.Console, Version=10.0.0.0, Culture=neutral, PublicKeyToken=b03f5f7f11d50a3a
// /usr/local/share/dotnet/packs/Microsoft.NETCore.App.Ref/10.0.2/ref/net10.0/System.Console.dll
#endregion
```

So `ExternalEntity` can be keyed on a real assembly identity -- name,
version, culture, public key token -- and the second line, which is a
machine path, stays out of it. Two caveats: the `#region` label is
localized by the running SDK's UI language (the measurement above was
taken on a Korean SDK, where it reads `어셈블리`), so the identity has to
be read positionally after the `#region` token rather than by keyword;
and reading it means opening a generated temp file, which is acceptable
only because nothing from it is persisted as source.

### A new source file needs a project reload, not a document sync

Adding `src/Core/Fresh.cs` and referencing it from `App` did not become
resolvable through `didChangeWatchedFiles`, and did not become resolvable
through `textDocument/didOpen` on the new file either -- both requests
timed out rather than answering. A project's Compile item set comes from
MSBuild, so a membership change needs the project reloaded, which is a
heavier operation than an edit and has its own barrier
(`workspace/projectInitializationComplete`).

### Multi-target: representable, with an honest limitation

A `<TargetFrameworks>net10.0;netstandard2.0</TargetFrameworks>` project
loads, and the server answers from **one** target framework without
saying which: `#if NET10_0_OR_GREATER` resolved to the `net10.0` branch,
and `documentSymbol` showed a single `Only()`. Which TFM answered is not
observable at this boundary.

That is representable the way the task's second option allows -- record
the ambiguity explicitly as partial coverage -- rather than silently
unioning two semantic worlds.

## Candidate B — not built

The spike stopped before it. Candidate A satisfies every P0 *capability*,
so the selection rule points at it; and the trust conflict is not a
property of the transport, so Candidate B would not resolve it either.
