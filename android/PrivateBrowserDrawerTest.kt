package ai.mcpagent.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import java.net.URI

/** Local JVM regression tests for the pure native-drawer sizing/search policy.
 * Inject a JVM HTTPS verifier rather than calling Android's Uri.parse stub. */
class PrivateBrowserDrawerTest {
  private val secureUrl: (String) -> Boolean = { url ->
    try {
      val uri = URI(url)
      uri.scheme.equals("https", true) && !uri.host.isNullOrBlank() &&
        uri.userInfo == null && uri.port != 0 && !url.any(Char::isWhitespace)
    } catch (_: Exception) { false }
  }

  @Test fun handsetCoversAvailableWidthAndTabletLeavesChatVisible() {
    val drawer = PrivateBrowserPlugin.Companion::drawerWidthDp
    assertEquals(0, drawer(0))
    assertEquals(320, drawer(320))
    assertEquals(360, drawer(360))
    assertEquals(600, drawer(600))
    assertEquals(420, drawer(601))
    assertEquals(520, drawer(800))
    assertEquals(520, drawer(1200))
    assertEquals(520, drawer(2000))
  }

  @Test fun typedSecureUrlsAndBareHostsStayHttps() {
    val target = PrivateBrowserPlugin.Companion::addressOrSearch
    assertEquals("https://example.org/path?q=1", target(" https://example.org/path?q=1 ", secureUrl))
    assertEquals("https://example.org/path", target("example.org/path", secureUrl))
    assertEquals("https://www.example.org", target("www.example.org", secureUrl))
  }

  @Test fun searchTermsArePercentEncodedAndNeverRunAsScript() {
    val target = PrivateBrowserPlugin.Companion::addressOrSearch
    assertEquals("https://www.google.com/search?q=cat%20photos", target("cat photos", secureUrl))
    assertEquals("https://www.google.com/search?q=kucing%20%26%20anjing",
      target("kucing & anjing", secureUrl))
    assertEquals("https://www.google.com/search?q=a%2Bb%3F", target("a+b?", secureUrl))
    assertEquals("https://www.google.com/search?q=%E7%8C%AB", target("猫", secureUrl))
  }

  @Test fun nonHttpsSchemesAndMalformedAddressesFailClosed() {
    val target = PrivateBrowserPlugin.Companion::addressOrSearch
    for (blocked in listOf("", "   ", "http://example.org", "javascript:alert(1)",
      "data:text/html,abc", "file:///etc/passwd", "intent://example.com",
      "//example.org", "\\\\example.org", "https://example.org with spaces",
      "https://user:pass@example.org", "https://"))
      assertNull("Should reject: $blocked", target(blocked, secureUrl))
    assertNull(target("x".repeat(513), secureUrl))
  }

  @Test fun cleanupOnlyAcceptsPrivateBrowserGeneratedNames() {
    val validates = PrivateBrowserPlugin.Companion::isOwnedProfileName
    assertEquals(true, validates("cos_inapp_0123456789abcdef0123456789abcdef"))
    for (invalid in listOf("Default","cos_inapp_../Default",
      "cos_inapp_0123456789abcdef0123456789abcdef/other",
      "cos_inapp_0123456789abcdef0123456789abcdeG",
      "cos_inapp_0123456789abcdef0123456789abcde",
      "mcp_chatgpt_builtin_v1","")) {
      assertEquals("Must never delete profile $invalid", false, validates(invalid))
    }
  }

  @Test fun retainedTabListRequiresOneToFiveValidatedHttpsAddresses() {
    val valid = PrivateBrowserPlugin.Companion::retainedUrlsValid
    assertEquals(true, valid(listOf("https://example.org/"), secureUrl))
    assertEquals(true, valid((1..5).map { "https://example.org/tab$it" }, secureUrl))
    assertEquals(false, valid(emptyList(), secureUrl))
    assertEquals(false, valid((1..6).map { "https://example.org/tab$it" }, secureUrl))
    assertEquals(false, valid(listOf("https://example.org/", "http://example.org"), secureUrl))
    assertEquals(false, valid(listOf("https://example.org/\r\nscript"), secureUrl))
    assertEquals(false, valid(listOf("https://example.org/" + "a".repeat(2048)), secureUrl))
  }
}
