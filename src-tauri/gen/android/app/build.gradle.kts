import groovy.json.JsonSlurper
import java.io.File
import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

val releaseKeystorePropertiesFile = rootProject.file("keystore.properties")
val hasReleaseSigning = releaseKeystorePropertiesFile.exists()

android {
    compileSdk = 36
    namespace = "com.theaspirational.orgasmic"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.theaspirational.orgasmic"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                val keystoreProperties = Properties().apply {
                    releaseKeystorePropertiesFile.inputStream().use { load(it) }
                }

                keyAlias = keystoreProperties["keyAlias"] as String
                keyPassword = keystoreProperties["password"] as String
                storeFile = file(keystoreProperties["storeFile"] as String)
                storePassword = keystoreProperties["password"] as String
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {
                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            if (hasReleaseSigning) {
                signingConfig = signingConfigs.getByName("release")
            }
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../../ui"
}

// Locate the Kotlin/AAR component that `rustls-platform-verifier` (pulled in by
// reqwest's rustls stack, and used by tauri + tauri-plugin-updater) needs on
// Android. Without it the native verifier can't reach Android's trust store and
// every HTTPS request aborts the process. The crate ships the AAR inside the
// `rustls-platform-verifier-android` source dir; `cargo metadata` resolves that
// dir portably so no machine-specific registry path is hard-coded.
val rustlsPlatformVerifierMaven: File = run {
    val manifest = File(rootDir, "../../Cargo.toml").canonicalPath
    val metadataJson = providers.exec {
        commandLine(
            "cargo", "metadata", "--format-version", "1",
            "--filter-platform", "aarch64-linux-android",
            "--manifest-path", manifest,
        )
    }.standardOutput.asText.get()

    @Suppress("UNCHECKED_CAST")
    val packages = (JsonSlurper().parseText(metadataJson) as Map<String, Any?>)
        .getValue("packages") as List<Map<String, Any?>>
    val crateManifest = packages
        .first { it["name"] == "rustls-platform-verifier-android" }
        .getValue("manifest_path") as String
    File(File(crateManifest).parentFile, "maven")
}

repositories {
    maven {
        url = uri(rustlsPlatformVerifierMaven)
        metadataSources { artifact() }
    }
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    // Android trust-store glue for rustls-platform-verifier. The version tracks
    // the `rustls-platform-verifier-android` crate in Cargo.lock — bump together.
    // `@aar`: this Maven repo exposes only the artifact (no resolvable metadata),
    // so name the AAR explicitly or Gradle looks for a non-existent .jar. The
    // component is self-contained (its POM declares no transitive deps).
    implementation("rustls:rustls-platform-verifier:0.1.1@aar")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")
