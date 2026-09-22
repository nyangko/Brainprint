namespace Contracts;

public interface IRunner
{
    void Run();
}

public interface INamed
{
    string Name { get; }
}
