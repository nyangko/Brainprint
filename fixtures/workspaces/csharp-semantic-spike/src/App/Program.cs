using System;
using Contracts;
using Core;
using Alias = Contracts.Model;

namespace App;

public static class Program
{
    public static void Main()
    {
        var runner = new Runner();

        // Cross-project call into a partial type's second declaration.
        var computed = runner.Compute(1);

        // Virtual dispatch through a base-typed local: the declaration
        // the compiler binds is not what runs.
        BaseRunner based = runner;
        based.Run();

        // Non-virtual, statically bound.
        based.NotVirtual();

        // Exact overload selection.
        var a = Overloads.Parse("x");
        var b = Overloads.Parse(1);

        // Generic method and generic type.
        var c = Overloads.Convert(runner.Name);
        var boxed = Overloads.Wrap(new Alias());

        // Extension method.
        var label = runner.Label();

        // Framework type from a reference assembly.
        Console.WriteLine($"{computed}{a.Name}{b.Count}{c}{boxed.Value.Name}{label}");
    }
}
