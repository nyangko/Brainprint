using Contracts;

namespace Core;

public abstract class BaseRunner
{
    public virtual void Run() { }

    public abstract int Compute(int seed);

    public void NotVirtual() { }
}

// A same-name trap: an unrelated `Run` in another type, in another
// namespace. Nothing may resolve to it by name.
public sealed class Unrelated
{
    public void Run() { }
}
