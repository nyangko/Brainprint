using Contracts;

namespace Core;

public partial class Runner : BaseRunner, IRunner, INamed
{
    public override void Run() { }

    public string Name => "runner";
}
