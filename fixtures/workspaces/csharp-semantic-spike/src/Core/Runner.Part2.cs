using Contracts;

namespace Core;

public partial class Runner
{
    public override int Compute(int seed) => seed + Extra();

    private int Extra() => 1;

    // Explicit interface implementation: a same-name identity trap.
    void IRunner.Run() { }
}
