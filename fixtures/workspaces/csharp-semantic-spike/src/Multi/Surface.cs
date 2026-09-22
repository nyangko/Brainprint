namespace Multi;

// One source file, two semantic worlds. The declared type of `Chosen`
// differs per target framework, so the binding a semantic tier can
// anchor -- a field's declared type -- lands on a different declaration
// under each one. A backend that answers from one framework resolves
// one of these and says nothing about the other.
public static class Surface
{
#if NET10_0_OR_GREATER
    private static readonly Modern Chosen = new Modern();
#else
    private static readonly Legacy Chosen = new Legacy();
#endif

    public static string Which() => Chosen.Name;
}

public sealed class Modern
{
    public string Name => "net10.0";
}

public sealed class Legacy
{
    public string Name => "netstandard2.0";
}
