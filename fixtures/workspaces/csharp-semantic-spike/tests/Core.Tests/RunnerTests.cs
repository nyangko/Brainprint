using Contracts;
using Core;

namespace Core.Tests;

public sealed class RunnerTests
{
    // A declared type reference to a partial type, from another project.
    private readonly Runner _shared = new Runner();

    public bool ComputeReturnsSeedPlusExtra()
    {
        return _shared.Compute(1) == 2;
    }

    public bool ParseSelectsTheStringOverload()
    {
        return Overloads.Parse("x").Name == "x";
    }
}
