package ai.mcpagent.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** JVM-only checks of the security boundary. No Termux app or emulator needed. */
class TermuxRunPolicyTest {
  private inline fun rejects(block: () -> Unit) {
    var denied=false
    try { block() } catch (_: IllegalArgumentException) { denied=true }
    assertTrue("Expected fail-closed validation",denied)
  }

  @Test fun termuxResultVersionHasHardMinimum() {
    for (value in listOf("0.109","0.109.0","0.118.3","0.120","1.0.0")) {
      assertTrue("Expected supported version: $value",TermuxRunPolicy.versionSupportsResult(value))
    }
    for (value in listOf(null,"","0.95","0.108","0.108.99","0.109-beta",
      "0.109 malicious","0.0.0","v0.118.0")) {
      assertFalse("Expected unsupported version: $value",TermuxRunPolicy.versionSupportsResult(value))
    }
  }

  @Test fun nodeRunsOnlyOneTermuxOwnedJavaScriptEntry() {
    val spec=TermuxRunPolicy.fromTarget("node","run","site/index.js")
    assertEquals("/data/data/com.termux/files/usr/bin/node",spec.executable)
    assertEquals("/data/data/com.termux/files/home/projects/site",spec.workingDirectory)
    assertEquals(listOf("--","./index.js"),spec.arguments.toList())
    assertEquals(listOf("--check","./app.mjs"),
      TermuxRunPolicy.fromTarget("node","check","site/app.mjs").arguments.toList())
    assertEquals(listOf("--version"),
      TermuxRunPolicy.fromTarget("node","version",null).arguments.toList())
    for (bad in listOf("../index.js","site/../../index.js","/home/index.js",
      "site/.secret.js","site/--inspect.js","site/run.sh","site/foo.js;sh",
      "site/foo.js\u0000","site/sub/other.js","site/script.js\n--eval",
      "Site/index.js","site/.npmrc")) rejects {
      TermuxRunPolicy.fromTarget("node","run",bad)
    }
    rejects { TermuxRunPolicy.fromTarget("node","eval","site/index.js") }
    rejects { TermuxRunPolicy.fromTarget("node","version","site/index.js") }
  }

  @Test fun npmOnlyListsOrInstallsOfflineAndNeverExecutesScripts() {
    val install=TermuxRunPolicy.fromTarget("npm","install","site")
    assertEquals("/data/data/com.termux/files/usr/bin/npm",install.executable)
    assertEquals("/data/data/com.termux/files/home/projects/site",install.workingDirectory)
    assertEquals(listOf("install","--offline","--ignore-scripts","--no-audit","--no-fund"),
      install.arguments.toList())
    assertEquals(listOf("ls","--depth=0","--offline","--json"),
      TermuxRunPolicy.fromTarget("npm","list","site").arguments.toList())
    rejects { TermuxRunPolicy.fromTarget("npm","run","site") }
    rejects { TermuxRunPolicy.fromTarget("npm","exec","site") }
    rejects { TermuxRunPolicy.fromTarget("npm","install","site;curl evil") }
    rejects { TermuxRunPolicy.fromTarget("npm","list","../private") }
    rejects { TermuxRunPolicy.fromTarget("npm","version","site") }
  }

  @Test fun sshAllowsOnlyFixedSafeProbeWithStrictKnownHosts() {
    val ssh=TermuxRunPolicy.fromTarget("ssh","test","user@example.org:2222")
    assertEquals("/data/data/com.termux/files/usr/bin/ssh",ssh.executable)
    assertEquals("true",ssh.arguments.last())
    assertTrue(ssh.arguments.toList().contains("-oStrictHostKeyChecking=yes"))
    assertTrue(ssh.arguments.toList().contains("-oBatchMode=yes"))
    assertTrue(ssh.arguments.toList().contains("-oProxyCommand=none"))
    assertTrue(ssh.arguments.toList().contains("user@example.org"))
    assertTrue(ssh.arguments.toList().contains("2222"))
    for (bad in listOf("root@example.org:0","user@example.org:65536",
      "u@example.com;sh:22","u@bad..host:22","u@bad-host-.com:22",
      "u@host:22;curl","-oProxyCommand@host:22","u@[::1]:22",
      "u@host:22\nother","user@host:22/../../secret")) rejects {
      TermuxRunPolicy.fromTarget("ssh","test",bad)
    }
    rejects { TermuxRunPolicy.fromTarget("ssh","exec","u@host:22") }
    rejects { TermuxRunPolicy.fromTarget("ssh","version","u@host:22") }
  }

  @Test fun nonAllowlistedExecutablesAndFlagsAreRejected() {
    for (tool in listOf("sh","bash","python","curl","node --eval","npm exec",
      "ssh -oProxyCommand","/system/bin/sh","NODE")) {
      rejects { TermuxRunPolicy.fromTarget(tool,"version",null) }
    }
    rejects { TermuxRunPolicy.fromTarget("node","--version",null) }
    rejects { TermuxRunPolicy.fromTarget("node","run",null) }
  }

  @Test fun verifiedResultRequiresOriginalLengthsInternalSuccessAndExactExitCode() {
    val ok=TermuxRunPolicy.verifiedResult(0,-1,"v1.0.0\n","",7L,0L)
    assertTrue(ok.success)
    assertEquals(0,ok.exitCode)
    assertEquals("v1.0.0\n",ok.stdout)
    val failure=TermuxRunPolicy.verifiedResult(127,-1,"","not found",0L,9L)
    assertFalse(failure.success)
    assertEquals(127,failure.exitCode)
    rejects { TermuxRunPolicy.verifiedResult(null,-1,"","",0L,0L) }
    rejects { TermuxRunPolicy.verifiedResult(0,null,"","",0L,0L) }
    rejects { TermuxRunPolicy.verifiedResult(0,1,"","",0L,0L) }
    rejects { TermuxRunPolicy.verifiedResult(0,-1,"","",null,0L) }
    rejects { TermuxRunPolicy.verifiedResult(0,-1,"","",0L,null) }
    rejects { TermuxRunPolicy.verifiedResult(0,-1,"abc","",2L,0L) }
    rejects { TermuxRunPolicy.verifiedResult(0,-1,"abc","",8L,0L) }
    rejects { TermuxRunPolicy.verifiedResult(0,-1,"abc","",20000L,0L) }
    rejects { TermuxRunPolicy.verifiedResult(0,-1,"x".repeat(17_000),"",17_000L,0L) }
    rejects { TermuxRunPolicy.verifiedResult(0,-1,"","",0L,-1L) }
  }
}
