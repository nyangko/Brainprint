namespace Core;

public class Middle : BaseRunner
{
    public override void Run() { }
    public override int Compute(int seed) => seed;
}

public sealed class Leaf : Middle
{
    public sealed override void Run() { }
}
