// The root build declares the plugins and applies none of them. Everything real lives in :app.
buildscript {
    dependencies {
        // AGP 9 compiles Kotlin with a Kotlin Gradle plugin it brings along, older than the
        // catalog's. A newer one on the classpath wins, and it has to be the release of the
        // compiler plugins below.
        classpath(libs.kotlin.gradle.plugin)
    }
}

plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.kotlin.compose) apply false
    alias(libs.plugins.kotlin.serialization) apply false
}
