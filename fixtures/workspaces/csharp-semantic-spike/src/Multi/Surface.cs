namespace Multi;

// One source file, two semantic worlds. `Which()` binds to a different
// declaration under each target framework, so a backend that answers
// from one of them is answering about one world -- and saying which one
// is the whole multi-target question.
public static class Surface
{
#if NET10_0_OR_GREATER
    public static string Which() => Modern.Name;
#else
    public static string Which() => Legacy.Name;
#endif
}

public static class Modern
{
    public const string Name = "net10.0";
}

public static class Legacy
{
    public const string Name = "netstandard2.0";
}
