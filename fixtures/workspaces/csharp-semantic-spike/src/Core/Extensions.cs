namespace Core;

public static class RunnerExtensions
{
    public static string Label(this Runner runner) => runner.Name;
}

// Another `Label`, on an unrelated type. The extension call must pick
// the one Roslyn selected, not the one that shares a name.
public static class UnrelatedExtensions
{
    public static string Label(this Unrelated unrelated) => "unrelated";
}
