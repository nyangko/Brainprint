using Contracts;

namespace Core;

public static class Overloads
{
    public static Model Parse(string value) => new Model { Name = value };

    public static Model Parse(int value) => new Model { Count = value };

    public static Model Parse(object value) => new Model();

    public static T Convert<T>(T value) => value;

    public static Box<Model> Wrap(Model model) => new Box<Model>(model);
}
