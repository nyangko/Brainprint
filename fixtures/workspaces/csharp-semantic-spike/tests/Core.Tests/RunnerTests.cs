using Contracts;
using Core;

namespace Core.Tests;

public sealed class RunnerTests
{
    public bool ComputeReturnsSeedPlusExtra()
    {
        var runner = new Runner();
        return runner.Compute(1) == 2;
    }

    public bool ParseSelectsTheStringOverload()
    {
        return Overloads.Parse("x").Name == "x";
    }
}
