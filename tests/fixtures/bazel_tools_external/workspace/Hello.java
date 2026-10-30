public class Hello {
    public static void main(String[] args) {
        System.out.println("hello from java -jar " + String.join(" ", args) + " java.home-tail=" + new java.io.File(System.getProperty("java.home")).getName());
    }
}
