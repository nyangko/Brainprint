#!/usr/bin/env bash
# Pin the official Roslyn language server locally. Never PATH, never
# global, and never run from Brainprint production code.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
version="5.4.0-2.26179.14"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) rid="osx-arm64" ;;
  Darwin-x86_64) rid="osx-x64" ;;
  Linux-aarch64) rid="linux-arm64" ;;
  Linux-x86_64) rid="linux-x64" ;;
  *) echo "no pinned Roslyn language server for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

cat > "$here/pin.csproj" <<PROJ
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net10.0</TargetFramework>
    <RestorePackagesPath>\$(MSBuildThisFileDirectory)packages</RestorePackagesPath>
    <EnableDefaultCompileItems>false</EnableDefaultCompileItems>
    <NoWarn>NU1503;NU5104</NoWarn>
  </PropertyGroup>
  <ItemGroup>
    <PackageDownload Include="Microsoft.CodeAnalysis.LanguageServer.$rid" Version="[$version]" />
  </ItemGroup>
</Project>
PROJ

dotnet restore "$here/pin.csproj"
echo "server: $here/packages/microsoft.codeanalysis.languageserver.$rid/$version/content/LanguageServer/$rid/Microsoft.CodeAnalysis.LanguageServer"
