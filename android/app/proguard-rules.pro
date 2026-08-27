# R8 is off for both build types, see the release block in app/build.gradle.kts for why. Nothing here
# runs today. It is kept so that turning shrinking on later starts from something that works, rather
# than from a handshake that fails to agree on any algorithm and says nothing about the reason.
#
# Almost all of it is the ssh client. sshj picks its ciphers, key exchanges, MACs and key formats out
# of name-keyed factory lists and then reaches the actual crypto through the JCE by algorithm name.
# R8 sees none of that, so it removes the implementations and leaves the lookups behind.

# The ssh stack, kept whole. It is small and shrinking it is not worth the class of bug it produces.
-keep class net.schmizz.sshj.** { *; }
-keep class net.i2p.crypto.eddsa.** { *; }
-keep class org.bouncycastle.** { *; }
# asn-one, which sshj pulls in to parse key blobs.
-keep class com.hierynomus.** { *; }

# JCE providers are instantiated by name, and the no-argument constructor is the only way in.
-keepclassmembers class * extends java.security.Provider {
    public <init>();
}
# Same for the sshj factories, which are listed by name and built reflectively.
-keepclassmembers class * implements net.schmizz.sshj.common.Factory$Named {
    public <init>(...);
}

# slf4j finds its binding by loading a fixed class name. Strip it and sshj's logging quietly stops
# reaching logcat, which is exactly when it is wanted.
-keep class org.slf4j.impl.** { *; }
-keep class org.slf4j.** { *; }

# Bouncy Castle and sshj reference JDK and optional third-party classes that Android does not have.
-dontwarn javax.naming.**
-dontwarn org.slf4j.**
-dontwarn com.jcraft.jzlib.**
-dontwarn org.bouncycastle.**
-dontwarn net.schmizz.sshj.**
-dontwarn com.hierynomus.**
-dontwarn java.lang.invoke.**

# The JCE reads annotations and generic signatures at runtime, and kotlinx.serialization needs the
# same two to match a class to its generated serializer.
-keepattributes *Annotation*,Signature,InnerClasses,EnclosingMethod,Exceptions

# Keep a crash off the phone readable, but rename the source file so the mapping still means
# something.
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile
